// StartOS entry point. Glues every module together so `start-cli` can pack
// the package.

import { sdk } from './sdk'

import { actions } from './actions'
import { createBackup, restoreBackup } from './backups'
import { setDependencies } from './dependencies'
import { initFn, uninitFn } from './init'
import { setInterfaces } from './interfaces'
import { main } from './main'
import { manifest } from './manifest'
import { versions } from './versions'

export const { packageInit, packageUninit, containerInit } = sdk.setupPackageInit({
  init: initFn,
  uninit: uninitFn,
})

export {
  manifest,
  main,
  actions,
  setDependencies,
  setInterfaces,
  createBackup,
  restoreBackup,
  versions,
}
