use std::borrow::{Borrow, Cow};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use jsonpath_lib::Compiled as CompiledJsonPath;
use patch_db::{DbHandle, OptionModel};
use rand::{CryptoRng, Rng};
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Number, Value};
use sqlx::SqlitePool;

use super::util::{self, CharSet, NumRange, UniqueBy, STATIC_NULL};
use super::{Config, MatchError, NoMatchWithPath, TimeoutError, TypeOf};
use crate::config::ConfigurationError;
use crate::context::RpcContext;
use crate::net::interface::InterfaceId;
use crate::s9pk::manifest::{Manifest, PackageId};
use crate::Error;

// Config Value Specifications
#[async_trait]
pub trait ValueSpec {
    // This function defines whether the value supplied in the argument is
    // consistent with the spec in &self
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath>;
    // This function checks whether the value spec is consistent with itself,
    // since not all inVariant can be checked by the type
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath>;
    // update is to fill in values for environment pointers recursively
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError>;
    // returns all pointers that are live in the provided config
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath>;
    // requires returns whether the app id is the target of a pointer within it
    fn requires(&self, id: &PackageId, value: &Value) -> bool;
    // defines if 2 values of this type are equal for the purpose of uniqueness
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool;
}

// Config Value Default Generation
//
// This behavior is defined by two independent traits as well as a third that
// represents a conjunction of those two traits:
//
// DefaultableWith - defines an associated type describing the information it
// needs to be able to generate a default value, as well as a function for
// extracting relevant pieces of that information and using it to actually
// generate the default value
//
// HasDefaultSpec - only purpose is to summon the default spec for the type
//
// Defaultable - this is a redundant trait that may replace 'DefaultableWith'
// and 'HasDefaultSpec'.
pub trait DefaultableWith {
    type DefaultSpec: Sync;
    type Error: std::error::Error;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error>;
}
pub trait HasDefaultSpec: DefaultableWith {
    fn default_spec(&self) -> &Self::DefaultSpec;
}

pub trait Defaultable {
    type Error;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error>;
}
impl<T, E> Defaultable for T
where
    T: HasDefaultSpec + DefaultableWith<Error = E> + Sync,
    E: std::error::Error,
{
    type Error = E;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.gen_with(self.default_spec().borrow(), rng, timeout)
    }
}

// WithDefault - trivial wrapper that pairs a 'DefaultableWith' type with a
// default spec
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WithDefault<T: DefaultableWith> {
    #[serde(flatten)]
    pub inner: T,
    pub default: T::DefaultSpec,
}
impl<T> DefaultableWith for WithDefault<T>
where
    T: DefaultableWith + Sync + Send,
    T::DefaultSpec: Send,
{
    type DefaultSpec = T::DefaultSpec;
    type Error = T::Error;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.inner.gen_with(spec, rng, timeout)
    }
}
impl<T> HasDefaultSpec for WithDefault<T>
where
    T: DefaultableWith + Sync + Send,
    T::DefaultSpec: Send,
{
    fn default_spec(&self) -> &Self::DefaultSpec {
        &self.default
    }
}
#[async_trait]
impl<T> ValueSpec for WithDefault<T>
where
    T: ValueSpec + DefaultableWith + Send + Sync,
    Self: Send + Sync,
{
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        self.inner.matches(value)
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        self.inner.validate(manifest)
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        self.inner
            .update(ctx, db, manifest, config_overrides, value)
            .await
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        self.inner.pointers(value)
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        self.inner.requires(id, value)
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        self.inner.eq(lhs, rhs)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WithNullable<T> {
    #[serde(flatten)]
    pub inner: T,
    pub nullable: bool,
}
#[async_trait]
impl<T> ValueSpec for WithNullable<T>
where
    T: ValueSpec + Send + Sync,
    Self: Send + Sync,
{
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match (self.nullable, value) {
            (true, &Value::Null) => Ok(()),
            _ => self.inner.matches(value),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        self.inner.validate(manifest)
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        self.inner
            .update(ctx, db, manifest, config_overrides, value)
            .await
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        self.inner.pointers(value)
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        self.inner.requires(id, value)
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        self.inner.eq(lhs, rhs)
    }
}

impl<T> DefaultableWith for WithNullable<T>
where
    T: DefaultableWith + Sync + Send,
{
    type DefaultSpec = T::DefaultSpec;
    type Error = T::Error;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.inner.gen_with(spec, rng, timeout)
    }
}

impl<T> Defaultable for WithNullable<T>
where
    T: Defaultable + Sync + Send,
{
    type Error = T::Error;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.inner.gen(rng, timeout)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WithDescription<T> {
    #[serde(flatten)]
    pub inner: T,
    pub description: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
#[async_trait]
impl<T> ValueSpec for WithDescription<T>
where
    T: ValueSpec + Sync + Send,
    Self: Sync + Send,
{
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        self.inner.matches(value)
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        self.inner.validate(manifest)
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        self.inner
            .update(ctx, db, manifest, config_overrides, value)
            .await
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        self.inner.pointers(value)
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        self.inner.requires(id, value)
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        self.inner.eq(lhs, rhs)
    }
}

impl<T> DefaultableWith for WithDescription<T>
where
    T: DefaultableWith + Sync + Send,
{
    type DefaultSpec = T::DefaultSpec;
    type Error = T::Error;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.inner.gen_with(spec, rng, timeout)
    }
}

impl<T> Defaultable for WithDescription<T>
where
    T: Defaultable + Sync + Send,
{
    type Error = T::Error;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.inner.gen(rng, timeout)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum ValueSpecAny {
    Boolean(WithDescription<WithDefault<ValueSpecBoolean>>),
    Enum(WithDescription<WithDefault<ValueSpecEnum>>),
    List(ValueSpecList),
    Number(WithDescription<WithDefault<WithNullable<ValueSpecNumber>>>),
    Object(WithDescription<ValueSpecObject>),
    String(WithDescription<WithDefault<WithNullable<ValueSpecString>>>),
    Union(WithDescription<WithDefault<ValueSpecUnion>>),
    Pointer(WithDescription<ValueSpecPointer>),
}
impl ValueSpecAny {
    pub fn name<'a>(&'a self) -> &'a str {
        match self {
            ValueSpecAny::Boolean(b) => b.name.as_str(),
            ValueSpecAny::Enum(e) => e.name.as_str(),
            ValueSpecAny::List(l) => match l {
                ValueSpecList::Enum(e) => e.name.as_str(),
                ValueSpecList::Number(n) => n.name.as_str(),
                ValueSpecList::Object(o) => o.name.as_str(),
                ValueSpecList::String(s) => s.name.as_str(),
                ValueSpecList::Union(u) => u.name.as_str(),
            },
            ValueSpecAny::Number(n) => n.name.as_str(),
            ValueSpecAny::Object(o) => o.name.as_str(),
            ValueSpecAny::Pointer(p) => p.name.as_str(),
            ValueSpecAny::String(s) => s.name.as_str(),
            ValueSpecAny::Union(u) => u.name.as_str(),
        }
    }
}
#[async_trait]
impl ValueSpec for ValueSpecAny {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecAny::Boolean(a) => a.matches(value),
            ValueSpecAny::Enum(a) => a.matches(value),
            ValueSpecAny::List(a) => a.matches(value),
            ValueSpecAny::Number(a) => a.matches(value),
            ValueSpecAny::Object(a) => a.matches(value),
            ValueSpecAny::String(a) => a.matches(value),
            ValueSpecAny::Union(a) => a.matches(value),
            ValueSpecAny::Pointer(a) => a.matches(value),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecAny::Boolean(a) => a.validate(manifest),
            ValueSpecAny::Enum(a) => a.validate(manifest),
            ValueSpecAny::List(a) => a.validate(manifest),
            ValueSpecAny::Number(a) => a.validate(manifest),
            ValueSpecAny::Object(a) => a.validate(manifest),
            ValueSpecAny::String(a) => a.validate(manifest),
            ValueSpecAny::Union(a) => a.validate(manifest),
            ValueSpecAny::Pointer(a) => a.validate(manifest),
        }
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        match self {
            ValueSpecAny::Boolean(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::Enum(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::List(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::Number(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::Object(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::String(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::Union(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecAny::Pointer(a) => a.update(ctx, db, manifest, config_overrides, value).await,
        }
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        match self {
            ValueSpecAny::Boolean(a) => a.pointers(value),
            ValueSpecAny::Enum(a) => a.pointers(value),
            ValueSpecAny::List(a) => a.pointers(value),
            ValueSpecAny::Number(a) => a.pointers(value),
            ValueSpecAny::Object(a) => a.pointers(value),
            ValueSpecAny::String(a) => a.pointers(value),
            ValueSpecAny::Union(a) => a.pointers(value),
            ValueSpecAny::Pointer(a) => a.pointers(value),
        }
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        match self {
            ValueSpecAny::Boolean(a) => a.requires(id, value),
            ValueSpecAny::Enum(a) => a.requires(id, value),
            ValueSpecAny::List(a) => a.requires(id, value),
            ValueSpecAny::Number(a) => a.requires(id, value),
            ValueSpecAny::Object(a) => a.requires(id, value),
            ValueSpecAny::String(a) => a.requires(id, value),
            ValueSpecAny::Union(a) => a.requires(id, value),
            ValueSpecAny::Pointer(a) => a.requires(id, value),
        }
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match self {
            ValueSpecAny::Boolean(a) => a.eq(lhs, rhs),
            ValueSpecAny::Enum(a) => a.eq(lhs, rhs),
            ValueSpecAny::List(a) => a.eq(lhs, rhs),
            ValueSpecAny::Number(a) => a.eq(lhs, rhs),
            ValueSpecAny::Object(a) => a.eq(lhs, rhs),
            ValueSpecAny::String(a) => a.eq(lhs, rhs),
            ValueSpecAny::Union(a) => a.eq(lhs, rhs),
            ValueSpecAny::Pointer(a) => a.eq(lhs, rhs),
        }
    }
}
impl Defaultable for ValueSpecAny {
    type Error = ConfigurationError;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        match self {
            ValueSpecAny::Boolean(a) => a.gen(rng, timeout).map_err(crate::util::Never::absurd),
            ValueSpecAny::Enum(a) => a.gen(rng, timeout).map_err(crate::util::Never::absurd),
            ValueSpecAny::List(a) => a.gen(rng, timeout),
            ValueSpecAny::Number(a) => a.gen(rng, timeout).map_err(crate::util::Never::absurd),
            ValueSpecAny::Object(a) => a.gen(rng, timeout),
            ValueSpecAny::String(a) => a.gen(rng, timeout).map_err(ConfigurationError::from),
            ValueSpecAny::Union(a) => a.gen(rng, timeout),
            ValueSpecAny::Pointer(a) => a.gen(rng, timeout),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueSpecBoolean {}
#[async_trait]
impl ValueSpec for ValueSpecBoolean {
    fn matches(&self, val: &Value) -> Result<(), NoMatchWithPath> {
        match val {
            Value::Bool(_) => Ok(()),
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "boolean",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, _manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        _ctx: &RpcContext,
        _db: &mut Db,
        _manifest: &Manifest,
        _config_overrides: &BTreeMap<PackageId, Config>,
        _value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        Ok(())
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        Ok(BTreeSet::new())
    }
    fn requires(&self, _id: &PackageId, _value: &Value) -> bool {
        false
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::Bool(lhs), Value::Bool(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecBoolean {
    type DefaultSpec = bool;
    type Error = crate::util::Never;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Send>(
        &self,
        spec: &Self::DefaultSpec,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Bool(*spec))
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ValueSpecEnum {
    pub values: IndexSet<String>,
    pub value_names: BTreeMap<String, String>,
}
impl<'de> serde::de::Deserialize<'de> for ValueSpecEnum {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub struct _ValueSpecEnum {
            pub values: IndexSet<String>,
            #[serde(default)]
            pub value_names: BTreeMap<String, String>,
        }

        let mut r#enum = _ValueSpecEnum::deserialize(deserializer)?;
        for name in &r#enum.values {
            if !r#enum.value_names.contains_key(name) {
                r#enum.value_names.insert(name.clone(), name.clone());
            }
        }
        Ok(ValueSpecEnum {
            values: r#enum.values,
            value_names: r#enum.value_names,
        })
    }
}
#[async_trait]
impl ValueSpec for ValueSpecEnum {
    fn matches(&self, val: &Value) -> Result<(), NoMatchWithPath> {
        match val {
            Value::String(b) => {
                if self.values.contains(b) {
                    Ok(())
                } else {
                    Err(NoMatchWithPath::new(MatchError::Enum(
                        b.clone(),
                        self.values.clone(),
                    )))
                }
            }
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "string",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, _manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        _ctx: &RpcContext,
        _db: &mut Db,
        _manifest: &Manifest,
        _config_overrides: &BTreeMap<PackageId, Config>,
        _value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        Ok(())
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        Ok(BTreeSet::new())
    }
    fn requires(&self, _id: &PackageId, _value: &Value) -> bool {
        false
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::String(lhs), Value::String(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecEnum {
    type DefaultSpec = String;
    type Error = crate::util::Never;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Send>(
        &self,
        spec: &Self::DefaultSpec,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::String(spec.clone()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListSpec<T> {
    pub spec: T,
    pub range: NumRange<usize>,
}
#[async_trait]
impl<T> ValueSpec for ListSpec<T>
where
    T: ValueSpec + Sync + Send,
    Self: Sync + Send,
{
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match value {
            Value::Array(l) => {
                if !self.range.contains(&l.len()) {
                    Err(NoMatchWithPath {
                        path: Vec::new(),
                        error: MatchError::LengthMismatch(self.range.clone(), l.len()),
                    })
                } else {
                    l.iter()
                        .enumerate()
                        .map(|(i, v)| {
                            self.spec
                                .matches(v)
                                .map_err(|e| e.prepend(format!("{}", i)))?;
                            if l.iter()
                                .enumerate()
                                .any(|(i2, v2)| i != i2 && self.spec.eq(v, v2))
                            {
                                Err(NoMatchWithPath::new(MatchError::ListUniquenessViolation)
                                    .prepend(format!("{}", i)))
                            } else {
                                Ok(())
                            }
                        })
                        .collect()
                }
            }
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "list",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        self.spec.validate(manifest)
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        if let Value::Array(ref mut ls) = value {
            for (i, val) in ls.into_iter().enumerate() {
                match self
                    .spec
                    .update(ctx, db, manifest, config_overrides, val)
                    .await
                {
                    Err(ConfigurationError::NoMatch(e)) => {
                        Err(ConfigurationError::NoMatch(e.prepend(format!("{}", i))))
                    }
                    a => a,
                }?;
            }
            Ok(())
        } else {
            Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                MatchError::InvalidType("list", value.type_of()),
            )))
        }
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        Ok(BTreeSet::new())
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        if let Value::Array(ref ls) = value {
            ls.into_iter().any(|v| self.spec.requires(id, v))
        } else {
            false
        }
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::Array(lhs), Value::Array(rhs)) => {
                lhs.iter().zip_longest(rhs.iter()).all(|zip| match zip {
                    itertools::EitherOrBoth::Both(lhs, rhs) => lhs == rhs,
                    _ => false,
                })
            }
            _ => false,
        }
    }
}

impl<T> DefaultableWith for ListSpec<T>
where
    T: DefaultableWith + Sync + Send,
{
    type DefaultSpec = Vec<T::DefaultSpec>;
    type Error = T::Error;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        let mut res = Vec::new();
        for spec_member in spec.iter() {
            res.push(self.spec.gen_with(spec_member, rng, timeout)?);
        }
        Ok(Value::Array(res))
    }
}

unsafe impl Sync for ValueSpecObject {} // TODO: remove
unsafe impl Send for ValueSpecObject {} // TODO: remove
unsafe impl Sync for ValueSpecUnion {} // TODO: remove
unsafe impl Send for ValueSpecUnion {} // TODO: remove

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "subtype")]
pub enum ValueSpecList {
    Enum(WithDescription<WithDefault<ListSpec<ValueSpecEnum>>>),
    Number(WithDescription<WithDefault<ListSpec<ValueSpecNumber>>>),
    Object(WithDescription<WithDefault<ListSpec<ValueSpecObject>>>),
    String(WithDescription<WithDefault<ListSpec<ValueSpecString>>>),
    Union(WithDescription<WithDefault<ListSpec<WithDefault<ValueSpecUnion>>>>),
}
#[async_trait]
impl ValueSpec for ValueSpecList {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecList::Enum(a) => a.matches(value),
            ValueSpecList::Number(a) => a.matches(value),
            ValueSpecList::Object(a) => a.matches(value),
            ValueSpecList::String(a) => a.matches(value),
            ValueSpecList::Union(a) => a.matches(value),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecList::Enum(a) => a.validate(manifest),
            ValueSpecList::Number(a) => a.validate(manifest),
            ValueSpecList::Object(a) => a.validate(manifest),
            ValueSpecList::String(a) => a.validate(manifest),
            ValueSpecList::Union(a) => a.validate(manifest),
        }
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        match self {
            ValueSpecList::Enum(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecList::Number(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecList::Object(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecList::String(a) => a.update(ctx, db, manifest, config_overrides, value).await,
            ValueSpecList::Union(a) => a.update(ctx, db, manifest, config_overrides, value).await,
        }
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        match self {
            ValueSpecList::Enum(a) => a.pointers(value),
            ValueSpecList::Number(a) => a.pointers(value),
            ValueSpecList::Object(a) => a.pointers(value),
            ValueSpecList::String(a) => a.pointers(value),
            ValueSpecList::Union(a) => a.pointers(value),
        }
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        match self {
            ValueSpecList::Enum(a) => a.requires(id, value),
            ValueSpecList::Number(a) => a.requires(id, value),
            ValueSpecList::Object(a) => a.requires(id, value),
            ValueSpecList::String(a) => a.requires(id, value),
            ValueSpecList::Union(a) => a.requires(id, value),
        }
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match self {
            ValueSpecList::Enum(a) => a.eq(lhs, rhs),
            ValueSpecList::Number(a) => a.eq(lhs, rhs),
            ValueSpecList::Object(a) => a.eq(lhs, rhs),
            ValueSpecList::String(a) => a.eq(lhs, rhs),
            ValueSpecList::Union(a) => a.eq(lhs, rhs),
        }
    }
}

impl Defaultable for ValueSpecList {
    type Error = ConfigurationError;

    fn gen<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        match self {
            ValueSpecList::Enum(a) => a.gen(rng, timeout).map_err(crate::util::Never::absurd),
            ValueSpecList::Number(a) => a.gen(rng, timeout).map_err(crate::util::Never::absurd),
            ValueSpecList::Object(a) => {
                let mut ret = match a.gen(rng, timeout).unwrap() {
                    Value::Array(l) => l,
                    a => {
                        return Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                            MatchError::InvalidType("list", a.type_of()),
                        )))
                    }
                };
                while !(
                    a.inner.inner.range.start_bound(),
                    std::ops::Bound::Unbounded,
                )
                    .contains(&ret.len())
                {
                    ret.push(
                        a.inner
                            .inner
                            .spec
                            .gen(rng, timeout)
                            .map_err(ConfigurationError::from)?,
                    );
                }
                Ok(Value::Array(ret))
            }
            ValueSpecList::String(a) => a.gen(rng, timeout).map_err(ConfigurationError::from),
            ValueSpecList::Union(a) => a.gen(rng, timeout).map_err(ConfigurationError::from),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueSpecNumber {
    range: Option<NumRange<f64>>,
    #[serde(default)]
    integral: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    units: Option<String>,
}
#[async_trait]
impl ValueSpec for ValueSpecNumber {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match value {
            Value::Number(n) => {
                let n = n.as_f64().unwrap();
                if self.integral && n.floor() != n {
                    return Err(NoMatchWithPath::new(MatchError::NonIntegral(n)));
                }
                if let Some(range) = &self.range {
                    if !range.contains(&n) {
                        return Err(NoMatchWithPath::new(MatchError::OutOfRange(
                            range.clone(),
                            n,
                        )));
                    }
                }
                Ok(())
            }
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "object",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, _manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        _ctx: &RpcContext,
        _db: &mut Db,
        _manifest: &Manifest,
        _config_overrides: &BTreeMap<PackageId, Config>,
        _value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        Ok(())
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        Ok(BTreeSet::new())
    }
    fn requires(&self, _id: &PackageId, _value: &Value) -> bool {
        false
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::Number(lhs), Value::Number(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecNumber {
    type DefaultSpec = Option<Number>;
    type Error = crate::util::Never;

    fn gen_with<R: Rng + CryptoRng + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(spec
            .clone()
            .map(|s| Value::Number(s))
            .unwrap_or(Value::Null))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ValueSpecObject {
    pub spec: ConfigSpec,
    pub display_as: Option<String>,
    #[serde(default)]
    pub unique_by: UniqueBy,
}
#[async_trait]
impl ValueSpec for ValueSpecObject {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match value {
            Value::Object(o) => self.spec.matches(o),
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "object",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        self.spec.validate(manifest)
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        if let Value::Object(o) = value {
            self.spec
                .update(ctx, db, manifest, config_overrides, o)
                .await
        } else {
            Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                MatchError::InvalidType("object", value.type_of()),
            )))
        }
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        if let Value::Object(o) = value {
            self.spec.pointers(o)
        } else {
            Err(NoMatchWithPath::new(MatchError::InvalidType(
                "object",
                value.type_of(),
            )))
        }
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        if let Value::Object(o) = value {
            self.spec.requires(id, o)
        } else {
            false
        }
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::Object(lhs), Value::Object(rhs)) => self.unique_by.eq(lhs, rhs),
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecObject {
    type DefaultSpec = Config;
    type Error = crate::util::Never;

    fn gen_with<R: Rng + CryptoRng + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Object(spec.clone()))
    }
}
impl Defaultable for ValueSpecObject {
    type Error = ConfigurationError;

    fn gen<R: Rng + CryptoRng + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        self.spec.gen(rng, timeout).map(Value::Object)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigSpec(pub IndexMap<String, ValueSpecAny>);
impl ConfigSpec {
    pub fn matches(&self, value: &Config) -> Result<(), NoMatchWithPath> {
        for (key, val) in self.0.iter() {
            if let Some(v) = value.get(key) {
                val.matches(v).map_err(|e| e.prepend(key.clone()))?;
            } else {
                val.matches(&Value::Null)
                    .map_err(|e| e.prepend(key.clone()))?;
            }
        }
        Ok(())
    }

    pub fn gen<R: Rng + CryptoRng + Sync + Send>(
        &self,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Config, ConfigurationError> {
        let mut res = Config::new();
        for (key, val) in self.0.iter() {
            res.insert(key.clone(), val.gen(rng, timeout)?);
        }
        Ok(res)
    }

    pub fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        for (name, val) in &self.0 {
            val.validate(manifest)
                .map_err(|e| e.prepend(name.clone()))?;
        }
        Ok(())
    }

    pub async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        cfg: &mut Config,
    ) -> Result<(), ConfigurationError> {
        for (k, vs) in self.0.iter() {
            match cfg.get_mut(k) {
                None => {
                    let mut v = Value::Null;
                    vs.update(ctx, db, manifest, config_overrides, &mut v)
                        .await?;
                    cfg.insert(k.clone(), v);
                }
                Some(v) => match vs.update(ctx, db, manifest, config_overrides, v).await {
                    Err(ConfigurationError::NoMatch(e)) => {
                        Err(ConfigurationError::NoMatch(e.prepend(k.clone())))
                    }
                    a => a,
                }?,
            };
        }
        Ok(())
    }

    pub fn pointers(&self, cfg: &Config) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        cfg.iter()
            .filter_map(|(k, v)| self.0.get(k).map(|vs| (k, vs.pointers(v))))
            .fold(Ok(BTreeSet::<ValueSpecPointer>::new()), |acc, v| {
                match (acc, v) {
                    // propagate existing errors
                    (Err(e), _) => Err(e),
                    // create new error case
                    (Ok(_), (k, Err(e))) => Err(e.prepend(k.clone())),
                    // combine sets
                    (Ok(s0), (_, Ok(s1))) => Ok(BTreeSet::from_iter(s0.union(&s1).cloned())),
                }
            })
    }

    pub fn requires(&self, id: &PackageId, cfg: &Config) -> bool {
        self.0
            .iter()
            .any(|(k, v)| v.requires(id, cfg.get(k).unwrap_or(&STATIC_NULL)))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Pattern {
    #[serde(with = "util::serde_regex")]
    pub pattern: Regex,
    pub pattern_description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueSpecString {
    #[serde(flatten)]
    pub pattern: Option<Pattern>,
    #[serde(default)]
    pub copyable: bool,
    #[serde(default)]
    pub masked: bool,
}
#[async_trait]
impl ValueSpec for ValueSpecString {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match value {
            Value::String(s) => {
                if let Some(pattern) = &self.pattern {
                    if pattern.pattern.is_match(s) {
                        Ok(())
                    } else {
                        Err(NoMatchWithPath::new(MatchError::Pattern(
                            s.to_owned(),
                            pattern.pattern.clone(),
                        )))
                    }
                } else {
                    Ok(())
                }
            }
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "string",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, _manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        _ctx: &RpcContext,
        _db: &mut Db,
        _manifest: &Manifest,
        _config_overrides: &BTreeMap<PackageId, Config>,
        _value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        Ok(())
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        Ok(BTreeSet::new())
    }
    fn requires(&self, _id: &PackageId, _value: &Value) -> bool {
        false
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::String(lhs), Value::String(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecString {
    type DefaultSpec = Option<DefaultString>;
    type Error = TimeoutError;

    fn gen_with<R: Rng + CryptoRng + Sync + Send + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, TimeoutError> {
        if let Some(spec) = spec {
            let now = timeout.as_ref().map(|_| std::time::Instant::now());
            loop {
                let candidate = spec.gen(rng);
                match (spec, &self.pattern) {
                    (DefaultString::Entropy(_), Some(pattern))
                        if !pattern.pattern.is_match(&candidate) =>
                    {
                        ()
                    }
                    _ => {
                        return Ok(Value::String(candidate));
                    }
                }
                if let (Some(now), Some(timeout)) = (now, timeout) {
                    if &now.elapsed() > timeout {
                        return Err(TimeoutError);
                    }
                }
            }
        } else {
            Ok(Value::Null)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DefaultString {
    Literal(String),
    Entropy(Entropy),
}
impl DefaultString {
    pub fn gen<R: Rng + CryptoRng + Sync + Send>(&self, rng: &mut R) -> String {
        match self {
            DefaultString::Literal(s) => s.clone(),
            DefaultString::Entropy(e) => e.gen(rng),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entropy {
    pub charset: Option<CharSet>,
    pub len: usize,
}
impl Entropy {
    pub fn gen<R: Rng + CryptoRng + Sync + Send>(&self, rng: &mut R) -> String {
        let len = self.len;
        let set = self
            .charset
            .as_ref()
            .map(|cs| Cow::Borrowed(cs))
            .unwrap_or_else(|| Cow::Owned(Default::default()));
        std::iter::repeat_with(|| set.gen(rng)).take(len).collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct UnionTag {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub variant_names: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ValueSpecUnion {
    pub tag: UnionTag,
    pub variants: BTreeMap<String, ConfigSpec>,
    pub display_as: Option<String>,
    pub unique_by: UniqueBy,
}

impl<'de> serde::de::Deserialize<'de> for ValueSpecUnion {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        #[serde(untagged)]
        pub enum _UnionTag {
            Old(String),
            New(UnionTag),
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub struct _ValueSpecUnion {
            pub variants: BTreeMap<String, ConfigSpec>,
            pub tag: _UnionTag,
            pub display_as: Option<String>,
            #[serde(default)]
            pub unique_by: UniqueBy,
        }

        let u = _ValueSpecUnion::deserialize(deserializer)?;
        Ok(ValueSpecUnion {
            tag: match u.tag {
                _UnionTag::Old(id) => UnionTag {
                    id: id.clone(),
                    name: id,
                    description: None,
                    variant_names: u
                        .variants
                        .keys()
                        .map(|k| (k.to_owned(), k.to_owned()))
                        .collect(),
                },
                _UnionTag::New(UnionTag {
                    id,
                    name,
                    description,
                    mut variant_names,
                }) => UnionTag {
                    id,
                    name,
                    description,
                    variant_names: {
                        let mut iter = u.variants.keys();
                        while variant_names.len() < u.variants.len() {
                            if let Some(variant) = iter.next() {
                                variant_names.insert(variant.to_owned(), variant.to_owned());
                            } else {
                                break;
                            }
                        }
                        variant_names
                    },
                },
            },
            variants: u.variants,
            display_as: u.display_as,
            unique_by: u.unique_by,
        })
    }
}

#[async_trait]
impl ValueSpec for ValueSpecUnion {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match value {
            Value::Object(o) => {
                if let Some(Value::String(ref tag)) = o.get(&self.tag.id) {
                    if let Some(obj_spec) = self.variants.get(tag) {
                        let mut without_tag = o.clone();
                        without_tag.remove(&self.tag.id);
                        obj_spec.matches(&without_tag)
                    } else {
                        Err(NoMatchWithPath::new(MatchError::Union(
                            tag.clone(),
                            self.variants.keys().cloned().collect(),
                        )))
                    }
                } else {
                    Err(NoMatchWithPath::new(MatchError::MissingTag(
                        self.tag.id.clone(),
                    )))
                }
            }
            Value::Null => Err(NoMatchWithPath::new(MatchError::NotNullable)),
            a => Err(NoMatchWithPath::new(MatchError::InvalidType(
                "object",
                a.type_of(),
            ))),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        for (name, variant) in &self.variants {
            if variant.0.get(&self.tag.id).is_some() {
                return Err(NoMatchWithPath::new(MatchError::PropertyMatchesUnionTag(
                    self.tag.id.clone(),
                    name.clone(),
                )));
            }
            variant.validate(manifest)?;
        }
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        if let Value::Object(o) = value {
            match o.get(&self.tag.id) {
                None => Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                    MatchError::MissingTag(self.tag.id.clone()),
                ))),
                Some(Value::String(tag)) => match self.variants.get(tag) {
                    None => Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                        MatchError::Union(tag.clone(), self.variants.keys().cloned().collect()),
                    ))),
                    Some(spec) => spec.update(ctx, db, manifest, config_overrides, o).await,
                },
                Some(other) => Err(ConfigurationError::NoMatch(
                    NoMatchWithPath::new(MatchError::InvalidType("string", other.type_of()))
                        .prepend(self.tag.id.clone()),
                )),
            }
        } else {
            Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                MatchError::InvalidType("object", value.type_of()),
            )))
        }
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        if let Value::Object(o) = value {
            match o.get(&self.tag.id) {
                None => Err(NoMatchWithPath::new(MatchError::MissingTag(
                    self.tag.id.clone(),
                ))),
                Some(Value::String(tag)) => match self.variants.get(tag) {
                    None => Err(NoMatchWithPath::new(MatchError::Union(
                        tag.clone(),
                        self.variants.keys().cloned().collect(),
                    ))),
                    Some(spec) => spec.pointers(o),
                },
                Some(other) => Err(NoMatchWithPath::new(MatchError::InvalidType(
                    "string",
                    other.type_of(),
                ))
                .prepend(self.tag.id.clone())),
            }
        } else {
            Err(NoMatchWithPath::new(MatchError::InvalidType(
                "object",
                value.type_of(),
            )))
        }
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        if let Value::Object(o) = value {
            match o.get(&self.tag.id) {
                Some(Value::String(tag)) => match self.variants.get(tag) {
                    None => false,
                    Some(spec) => spec.requires(id, o),
                },
                _ => false,
            }
        } else {
            false
        }
    }
    fn eq(&self, lhs: &Value, rhs: &Value) -> bool {
        match (lhs, rhs) {
            (Value::Object(lhs), Value::Object(rhs)) => self.unique_by.eq(lhs, rhs),
            _ => false,
        }
    }
}
impl DefaultableWith for ValueSpecUnion {
    type DefaultSpec = String;
    type Error = ConfigurationError;

    fn gen_with<R: Rng + CryptoRng + Sync + Send>(
        &self,
        spec: &Self::DefaultSpec,
        rng: &mut R,
        timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        let variant = if let Some(v) = self.variants.get(spec) {
            v
        } else {
            return Err(ConfigurationError::NoMatch(NoMatchWithPath::new(
                MatchError::Union(spec.clone(), self.variants.keys().cloned().collect()),
            )));
        };
        let cfg_res = variant.gen(rng, timeout)?;

        let mut tagged_cfg = Config::new();
        tagged_cfg.insert(self.tag.id.clone(), Value::String(spec.clone()));
        tagged_cfg.extend(cfg_res.into_iter());

        Ok(Value::Object(tagged_cfg))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "subtype")]
#[serde(rename_all = "kebab-case")]
pub enum ValueSpecPointer {
    Package(PackagePointerSpec),
    System(SystemPointerSpec),
}
impl fmt::Display for ValueSpecPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueSpecPointer::Package(p) => write!(f, "{}", p),
            ValueSpecPointer::System(p) => write!(f, "{}", p),
        }
    }
}
impl Defaultable for ValueSpecPointer {
    type Error = ConfigurationError;
    fn gen<R: Rng + CryptoRng + Sync + Send>(
        &self,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Null)
    }
}
#[async_trait]
impl ValueSpec for ValueSpecPointer {
    fn matches(&self, value: &Value) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecPointer::Package(a) => a.matches(value),
            ValueSpecPointer::System(a) => a.matches(value),
        }
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        match self {
            ValueSpecPointer::Package(a) => a.validate(manifest),
            ValueSpecPointer::System(a) => a.validate(manifest),
        }
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        match self {
            ValueSpecPointer::Package(a) => {
                a.update(ctx, db, manifest, config_overrides, value).await
            }
            ValueSpecPointer::System(a) => {
                a.update(ctx, db, manifest, config_overrides, value).await
            }
        }
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        let mut pointers = BTreeSet::new();
        pointers.insert(self.clone());
        Ok(pointers)
    }
    fn requires(&self, id: &PackageId, value: &Value) -> bool {
        match self {
            ValueSpecPointer::Package(a) => a.requires(id, value),
            ValueSpecPointer::System(a) => a.requires(id, value),
        }
    }
    fn eq(&self, _lhs: &Value, _rhs: &Value) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "target")]
#[serde(rename_all = "kebab-case")]
pub enum PackagePointerSpec {
    TorKey(TorKeyPointer),
    TorAddress(TorAddressPointer),
    LanAddress(LanAddressPointer),
    Config(ConfigPointer),
}
impl PackagePointerSpec {
    pub fn package_id(&self) -> &PackageId {
        match self {
            PackagePointerSpec::TorKey(TorKeyPointer { package_id, .. }) => package_id,
            PackagePointerSpec::TorAddress(TorAddressPointer { package_id, .. }) => package_id,
            PackagePointerSpec::LanAddress(LanAddressPointer { package_id, .. }) => package_id,
            PackagePointerSpec::Config(ConfigPointer { package_id, .. }) => package_id,
        }
    }
    async fn deref<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
    ) -> Result<Value, ConfigurationError> {
        match &self {
            PackagePointerSpec::TorKey(key) => key.deref(&manifest.id, &ctx.secret_store).await,
            PackagePointerSpec::TorAddress(tor) => tor.deref(db).await,
            PackagePointerSpec::LanAddress(lan) => lan.deref(db).await,
            PackagePointerSpec::Config(cfg) => cfg.deref(ctx, db, config_overrides).await,
        }
    }
}
impl fmt::Display for PackagePointerSpec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PackagePointerSpec::TorKey(key) => write!(f, "{}", key),
            PackagePointerSpec::TorAddress(tor) => write!(f, "{}", tor),
            PackagePointerSpec::LanAddress(lan) => write!(f, "{}", lan),
            PackagePointerSpec::Config(cfg) => write!(f, "{}", cfg),
        }
    }
}
impl Defaultable for PackagePointerSpec {
    type Error = ConfigurationError;
    fn gen<R: Rng + CryptoRng + Sync + Send>(
        &self,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Null)
    }
}
#[async_trait]
impl ValueSpec for PackagePointerSpec {
    fn matches(&self, _value: &Value) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    fn validate(&self, manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        if &manifest.id != self.package_id()
            && !manifest.dependencies.0.contains_key(self.package_id())
        {
            return Err(NoMatchWithPath::new(MatchError::InvalidPointer(
                ValueSpecPointer::Package(self.clone()),
            )));
        }
        match self {
            _ => Ok(()),
        }
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        manifest: &Manifest,
        config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        *value = self.deref(ctx, db, manifest, config_overrides).await?;
        Ok(())
    }
    fn pointers(&self, _value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        let mut pointers = BTreeSet::new();
        pointers.insert(ValueSpecPointer::Package(self.clone()));
        Ok(pointers)
    }
    fn requires(&self, id: &PackageId, _value: &Value) -> bool {
        self.package_id() == id
    }
    fn eq(&self, _lhs: &Value, _rhs: &Value) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TorAddressPointer {
    package_id: PackageId,
    interface: InterfaceId,
}
impl TorAddressPointer {
    async fn deref<Db: DbHandle>(&self, db: &mut Db) -> Result<Value, ConfigurationError> {
        let addr = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(&self.package_id)
            .and_then(|pde| pde.installed())
            .and_then(|installed| installed.interface_addresses().idx_model(&self.interface))
            .and_then(|addresses| addresses.tor_address())
            .get(db, true)
            .await
            .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
        Ok(addr.to_owned().map(Value::String).unwrap_or(Value::Null))
    }
}
impl fmt::Display for TorAddressPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TorAddressPointer {
                package_id,
                interface,
            } => write!(f, "{}: tor-address: {}", package_id, interface),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LanAddressPointer {
    package_id: PackageId,
    interface: InterfaceId,
}
impl fmt::Display for LanAddressPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanAddressPointer {
                package_id,
                interface,
            } => write!(f, "{}: lan-address: {}", package_id, interface),
        }
    }
}
impl LanAddressPointer {
    async fn deref<Db: DbHandle>(&self, db: &mut Db) -> Result<Value, ConfigurationError> {
        let addr = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(&self.package_id)
            .and_then(|pde| pde.installed())
            .and_then(|installed| installed.interface_addresses().idx_model(&self.interface))
            .and_then(|addresses| addresses.lan_address())
            .get(db, true)
            .await
            .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
        Ok(addr.to_owned().map(Value::String).unwrap_or(Value::Null))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConfigPointer {
    package_id: PackageId,
    selector: Arc<ConfigSelector>,
    multi: bool,
}
impl ConfigPointer {
    pub fn select(&self, val: &Value) -> Value {
        self.selector.select(self.multi, val)
    }
    async fn deref<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        config_overrides: &BTreeMap<PackageId, Config>,
    ) -> Result<Value, ConfigurationError> {
        if let Some(cfg) = config_overrides.get(&self.package_id) {
            Ok(self.select(&Value::Object(cfg.clone())))
        } else {
            let manifest_model: OptionModel<Manifest> = crate::db::DatabaseModel::new()
                .package_data()
                .idx_model(&self.package_id)
                .and_then(|pde| pde.installed())
                .map(|installed| installed.manifest())
                .into();
            let version = manifest_model
                .clone()
                .map(|manifest| manifest.version())
                .get(db, true)
                .await
                .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
            let cfg_actions = manifest_model
                .clone()
                .and_then(|manifest| manifest.config())
                .get(db, true)
                .await
                .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
            let volumes = manifest_model
                .map(|manifest| manifest.volumes())
                .get(db, true)
                .await
                .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
            if let (Some(version), Some(cfg_actions), Some(volumes)) =
                (&*version, &*cfg_actions, &*volumes)
            {
                let cfg_res = cfg_actions
                    .get(&ctx, &self.package_id, version, volumes)
                    .await
                    .map_err(|e| ConfigurationError::SystemError(Error::from(e)))?;
                if let Some(cfg) = cfg_res.config {
                    Ok(self.select(&Value::Object(cfg)))
                } else {
                    Ok(Value::Null)
                }
            } else {
                Ok(Value::Null)
            }
        }
    }
}
impl fmt::Display for ConfigPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigPointer {
                package_id,
                selector,
                ..
            } => write!(f, "{}: config: {}", package_id, selector),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigSelector {
    src: String,
    compiled: CompiledJsonPath,
}
impl ConfigSelector {
    fn select(&self, multi: bool, val: &Value) -> Value {
        let selected = self.compiled.select(&val).ok().unwrap_or_else(Vec::new);
        if multi {
            Value::Array(selected.into_iter().cloned().collect())
        } else {
            selected.get(0).map(|v| (*v).clone()).unwrap_or(Value::Null)
        }
    }
}
impl fmt::Display for ConfigSelector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.src)
    }
}
impl Serialize for ConfigSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.src)
    }
}
impl<'de> Deserialize<'de> for ConfigSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let src: String = Deserialize::deserialize(deserializer)?;
        let compiled = CompiledJsonPath::compile(&src).map_err(serde::de::Error::custom)?;
        Ok(Self { src, compiled })
    }
}
impl PartialEq for ConfigSelector {
    fn eq(&self, other: &ConfigSelector) -> bool {
        self.src == other.src
    }
}
impl Eq for ConfigSelector {}
impl PartialOrd for ConfigSelector {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.src.partial_cmp(&other.src)
    }
}
impl Ord for ConfigSelector {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.src.cmp(&other.src)
    }
}
impl Hash for ConfigSelector {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.src.hash(state)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TorKeyPointer {
    package_id: PackageId,
    interface: InterfaceId,
}
impl TorKeyPointer {
    async fn deref(
        &self,
        source_package: &PackageId,
        secrets: &SqlitePool,
    ) -> Result<Value, ConfigurationError> {
        if &self.package_id != source_package {
            return Err(ConfigurationError::PermissionDenied(
                ValueSpecPointer::Package(PackagePointerSpec::TorKey(self.clone())),
            ));
        }
        let x = sqlx::query!(
            "SELECT key FROM tor WHERE package = ? AND interface = ?",
            *self.package_id,
            *self.interface
        )
        .fetch_one(secrets)
        .await
        .map_err(|e| ConfigurationError::SystemError(e.into()))?;
        Ok(Value::String(base32::encode(
            base32::Alphabet::RFC4648 { padding: false },
            &x.key,
        )))
    }
}
impl fmt::Display for TorKeyPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: tor-key: {}", self.package_id, self.interface)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "target")]
pub enum SystemPointerSpec {}
impl fmt::Display for SystemPointerSpec {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SYSTEM: {}", match *self {})
    }
}
impl SystemPointerSpec {
    async fn deref<Db: DbHandle>(&self, db: &mut Db) -> Result<Value, ConfigurationError> {
        Ok(match *self {})
    }
}
impl Defaultable for SystemPointerSpec {
    type Error = ConfigurationError;
    fn gen<R: Rng + CryptoRng + Sync + Send>(
        &self,
        _rng: &mut R,
        _timeout: &Option<Duration>,
    ) -> Result<Value, Self::Error> {
        Ok(Value::Null)
    }
}
#[async_trait]
impl ValueSpec for SystemPointerSpec {
    fn matches(&self, _value: &Value) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    fn validate(&self, _manifest: &Manifest) -> Result<(), NoMatchWithPath> {
        Ok(())
    }
    async fn update<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        _manifest: &Manifest,
        _config_overrides: &BTreeMap<PackageId, Config>,
        value: &mut Value,
    ) -> Result<(), ConfigurationError> {
        *value = self.deref(db).await?;
        Ok(())
    }
    fn pointers(&self, value: &Value) -> Result<BTreeSet<ValueSpecPointer>, NoMatchWithPath> {
        let mut pointers = BTreeSet::new();
        pointers.insert(ValueSpecPointer::System(self.clone()));
        Ok(pointers)
    }
    fn requires(&self, _id: &PackageId, _value: &Value) -> bool {
        false
    }
    fn eq(&self, _lhs: &Value, _rhs: &Value) -> bool {
        false
    }
}

#[cfg(test)]
mod test {
    use rand::SeedableRng;

    use super::*;

    #[test]
    fn test_config() {
        let spec = serde_json::json!({
          "randomEnum": {
            "name": "Random Enum",
            "type": "enum",
            "default": "null",
            "description": "This is not even real.",
            "changeWarning": "Be careful chnaging this!",
            "values": [
              "null",
              "option1",
              "option2",
              "option3"
            ]
          },
          "testnet": {
            "name": "Testnet",
            "type": "boolean",
            "description": "determines whether your node is running ontestnet or mainnet",
            "changeWarning": "Chain will have to resync!",
            "default": false
          },
          "favoriteNumber": {
            "name": "Favorite Number",
            "type": "number",
            "integral": false,
            "description": "Your favorite number of all time",
            "changeWarning": "Once you set this number, it can never be changed without severe consequences.",
            "nullable": false,
            "default": 7,
            "range": "(-100,100]"
          },
          "secondaryNumbers": {
            "name": "Unlucky Numbers",
            "type": "list",
            "subtype": "number",
            "description": "Numbers that you like but are not your top favorite.",
            "spec": {
              "type": "number",
              "integral": false,
              "range": "[-100,200)"
            },
            "range": "[0,10]",
            "default": [
              2,
              3
            ]
          },
          "rpcsettings": {
            "name": "RPC Settings",
            "type": "object",
            "description": "rpc username and password",
            "changeWarning": "Adding RPC users gives them special permissions on your node.",
            "nullable": false,
            "nullByDefault": false,
            "spec": {
              "laws": {
                "name": "Laws",
                "type": "object",
                "description": "the law of the realm",
                "nullable": true,
                "nullByDefault": true,
                "spec": {
                  "law1": {
                    "name": "First Law",
                    "type": "string",
                    "description": "the first law",
                    "nullable": true
                  },
                  "law2": {
                    "name": "Second Law",
                    "type": "string",
                    "description": "the second law",
                    "nullable": true
                  }
                }
              },
              "rulemakers": {
                "name": "Rule Makers",
                "type": "list",
                "subtype": "object",
                "description": "the people who make the rules",
                "range": "[0,2]",
                "default": [],
                "spec": {
                  "type": "object",
                  "spec": {
                    "rulemakername": {
                      "name": "Rulemaker Name",
                      "type": "string",
                      "description": "the name of the rule maker",
                      "nullable": false,
                      "default": {
                        "charset": "a-g,2-9",
                        "len": 12
                      }
                    },
                    "rulemakerip": {
                      "name": "Rulemaker IP",
                      "type": "string",
                      "description": "the ip of the rule maker",
                      "nullable": false,
                      "default": "192.168.1.0",
                      "pattern": "^(([0-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-5])\\.){3}([0-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-5])$",
                      "patternDescription": "may only contain numbers and periods"
                    }
                  }
                }
              },
              "rpcuser": {
                "name": "RPC Username",
                "type": "string",
                "description": "rpc username",
                "nullable": false,
                "default": "defaultrpcusername",
                "pattern": "^[a-zA-Z]+$",
                "patternDescription": "must contain only letters."
              },
              "rpcpass": {
                "name": "RPC User Password",
                "type": "string",
                "description": "rpc password",
                "nullable": false,
                "default": {
                  "charset": "a-z,A-Z,2-9",
                  "len": 20
                }
              }
            }
          },
          "advanced": {
            "name": "Advanced",
            "type": "object",
            "description": "Advanced settings",
            "nullable": false,
            "nullByDefault": false,
            "spec": {
              "notifications": {
                "name": "Notification Preferences",
                "type": "list",
                "subtype": "enum",
                "description": "how you want to be notified",
                "range": "[1,3]",
                "default": [
                  "email"
                ],
                "spec": {
                  "type": "enum",
                  "values": [
                    "email",
                    "text",
                    "call",
                    "push",
                    "webhook"
                  ]
                }
              }
            }
          },
          "bitcoinNode": {
            "name": "Bitcoin Node Settings",
            "type": "union",
            "description": "The node settings",
            "default": "internal",
            "tag": {
                "id": "type",
                "name": "Type",
                "variantNames": {}
            },
            "Variant": {
              "internal": {
                "lan-address": {
                  "name": "LAN Address",
                  "type": "pointer",
                  "subtype": "package",
                  "target": "lan-address",
                  "package-id": "bitcoind",
                  "description": "the lan address"
                }
              },
              "external": {
                "public-domain": {
                  "name": "Public Domain",
                  "type": "string",
                  "description": "the public address of the node",
                  "nullable": false,
                  "default": "bitcoinnode.com",
                  "pattern": ".*",
                  "patternDescription": "anything"
                }
              }
            }
          },
          "port": {
            "name": "Port",
            "type": "number",
            "integral": true,
            "description": "the default port for your Bitcoin node. default: 8333, testnet: 18333, regtest: 18444",
            "nullable": true,
            "default": 8333,
            "range": "[0, 9999]",
            "units": "m/s"
          },
          "maxconnections": {
            "name": "Max Connections",
            "type": "string",
            "description": "the maximum number of commections allowed to your Bitcoin node",
            "nullable": true
          },
          "rpcallowip": {
            "name": "RPC Allowed IPs",
            "type": "list",
            "subtype": "string",
            "description": "external ip addresses that are authorized to access your Bitcoin node",
            "changeWarning": "Any IP you allow here will have RPC access to your Bitcoin node.",
            "range": "[1,10]",
            "default": [
              "192.168.1.1"
            ],
            "spec": {
              "type": "string",
              "pattern": "((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\\.){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])|((^(([0-9a-fA-F]{1,4}:){7,7}[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,7}:|([0-9a-fA-F]{1,4}:){1,6}:[0-9a-fA-F]{1,4}|([0-9a-fA-F]{1,4}:){1,5}(:[0-9a-fA-F]{1,4}){1,2}|([0-9a-fA-F]{1,4}:){1,4}(:[0-9a-fA-F]{1,4}){1,3}|([0-9a-fA-F]{1,4}:){1,3}(:[0-9a-fA-F]{1,4}){1,4}|([0-9a-fA-F]{1,4}:){1,2}(:[0-9a-fA-F]{1,4}){1,5}|[0-9a-fA-F]{1,4}:((:[0-9a-fA-F]{1,4}){1,6})|:((:[0-9a-fA-F]{1,4}){1,7}|:)|fe80:(:[0-9a-fA-F]{0,4}){0,4}%[0-9a-zA-Z]{1,}|::(ffff(:0{1,4}){0,1}:){0,1}((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9]).){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])|([0-9a-fA-F]{1,4}:){1,4}:((25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9]).){3,3}(25[0-5]|(2[0-4]|1{0,1}[0-9]){0,1}[0-9]))$)|(^[a-z2-7]{16}\\.onion$)|(^([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\\.)+[a-z0-9][a-z0-9-]{0,61}[a-z0-9]$))",
              "patternDescription": "must be a valid ipv4, ipv6, or domain name"
            }
          },
          "rpcauth": {
            "name": "RPC Auth",
            "type": "list",
            "subtype": "string",
            "description": "api keys that are authorized to access your Bitcoin node.",
            "range": "[0,*)",
            "default": [],
            "spec": {
              "type": "string"
            }
          }
        });
        let spec: ConfigSpec = serde_json::from_value(spec).unwrap();
        spec.validate(todo!()).unwrap();
        let config = spec
            .gen(&mut rand::rngs::StdRng::from_entropy(), &None)
            .unwrap();
        spec.matches(&config).unwrap();
    }
}
