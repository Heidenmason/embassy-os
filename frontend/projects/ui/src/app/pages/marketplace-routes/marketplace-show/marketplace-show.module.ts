import { NgModule } from '@angular/core'
import { CommonModule } from '@angular/common'
import { Routes, RouterModule } from '@angular/router'
import { IonicModule } from '@ionic/angular'
import {
  SharedPipesModule,
  EmverPipesModule,
  MarkdownPipeModule,
  TextSpinnerComponentModule,
} from '@start9labs/shared'
import { MarketplacePipesModule } from '@start9labs/marketplace'
import { InstallWizardComponentModule } from 'src/app/components/install-wizard/install-wizard.component.module'

import { MarketplaceStatusModule } from '../marketplace-status/marketplace-status.module'
import { MarketplaceShowPage } from './marketplace-show.page'
import { MarketplaceShowHeaderComponent } from './marketplace-show-header/marketplace-show-header.component'
import { MarketplaceShowDependentComponent } from './marketplace-show-dependent/marketplace-show-dependent.component'
import { MarketplaceShowDependenciesComponent } from './marketplace-show-dependencies/marketplace-show-dependencies.component'
import { MarketplaceShowAdditionalComponent } from './marketplace-show-additional/marketplace-show-additional.component'
import { MarketplaceShowAboutComponent } from './marketplace-show-about/marketplace-show-about.component'
import { MarketplaceShowControlsComponent } from './marketplace-show-controls/marketplace-show-controls.component'

const routes: Routes = [
  {
    path: '',
    component: MarketplaceShowPage,
  },
]

@NgModule({
  imports: [
    CommonModule,
    IonicModule,
    RouterModule.forChild(routes),
    TextSpinnerComponentModule,
    SharedPipesModule,
    EmverPipesModule,
    MarkdownPipeModule,
    MarketplacePipesModule,
    MarketplaceStatusModule,
    InstallWizardComponentModule,
  ],
  declarations: [
    MarketplaceShowPage,
    MarketplaceShowHeaderComponent,
    MarketplaceShowControlsComponent,
    MarketplaceShowDependentComponent,
    MarketplaceShowAboutComponent,
    MarketplaceShowDependenciesComponent,
    MarketplaceShowAdditionalComponent,
  ],
  exports: [
    MarketplaceShowPage,
    MarketplaceShowHeaderComponent,
    MarketplaceShowControlsComponent,
    MarketplaceShowDependentComponent,
    MarketplaceShowAboutComponent,
    MarketplaceShowDependenciesComponent,
    MarketplaceShowAdditionalComponent,
  ],
})
export class MarketplaceShowPageModule {}
