import { Injectable } from '@angular/core'
import * as aesjs from 'aes-js'
import * as pbkdf2 from 'pbkdf2'
import {
  HttpError,
  RpcError,
  HttpService,
  RPCOptions,
  Method,
  RPCResponse,
  isRpcError,
} from '@start9labs/shared'

@Injectable({
  providedIn: 'root',
})
export class RPCEncryptedService {
  productKey?: string

  constructor(private readonly http: HttpService) {}

  async rpcRequest<T>(opts: Omit<RPCOptions, 'timeout'>): Promise<T> {
    const encryptedBody = await AES_CTR.encryptPbkdf2(
      this.productKey || '',
      encodeUtf8(JSON.stringify(opts)),
    )

    const res: RPCResponse<T> = await this.http
      .httpRequest<ArrayBuffer>({
        method: Method.POST,
        url: this.http.relativeUrl,
        body: encryptedBody.buffer,
        responseType: 'arrayBuffer',
        headers: {
          'Content-Encoding': 'aesctr256',
          'Content-Type': 'application/json',
        },
      })
      .then(body => AES_CTR.decryptPbkdf2(this.productKey || '', body))
      .then(res => JSON.parse(res))
      .catch(e => {
        if (!e.status && !e.statusText) {
          throw new EncryptionError()
        } else {
          throw new HttpError(e)
        }
      })
    if (isRpcError(res)) throw new RpcError(res.error)
    return res.result
  }
}

class EncryptionError {
  readonly code = null
  readonly message = 'Invalid Key'
  readonly details = null
}

type AES_CTR = {
  encryptPbkdf2: (
    secretKey: string,
    messageBuffer: Uint8Array,
  ) => Promise<Uint8Array>
  decryptPbkdf2: (secretKey: string, arr: ArrayBuffer) => Promise<string>
}

const AES_CTR: AES_CTR = {
  encryptPbkdf2: async (secretKey: string, messageBuffer: Uint8Array) => {
    const salt = window.crypto.getRandomValues(new Uint8Array(16))
    const counter = window.crypto.getRandomValues(new Uint8Array(16))

    const key = pbkdf2.pbkdf2Sync(secretKey, salt, 1000, 256 / 8, 'sha256')

    const aesCtr = new aesjs.ModeOfOperation.ctr(
      key,
      new aesjs.Counter(counter),
    )
    const encryptedBytes = aesCtr.encrypt(messageBuffer)
    return new Uint8Array([...counter, ...salt, ...encryptedBytes])
  },
  decryptPbkdf2: async (secretKey: string, arr: ArrayBuffer) => {
    const buff = new Uint8Array(arr)
    const counter = buff.slice(0, 16)
    const salt = buff.slice(16, 32)

    const cipher = buff.slice(32)
    const key = pbkdf2.pbkdf2Sync(secretKey, salt, 1000, 256 / 8, 'sha256')

    const aesCtr = new aesjs.ModeOfOperation.ctr(
      key,
      new aesjs.Counter(counter),
    )
    const decryptedBytes = aesCtr.decrypt(cipher)

    return aesjs.utils.utf8.fromBytes(decryptedBytes)
  },
}

function encodeUtf8(str: string): Uint8Array {
  const encoder = new TextEncoder()
  return encoder.encode(str)
}
