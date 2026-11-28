# Changelog

## [0.10.0](https://github.com/will-molloy/video-tools/compare/v0.9.3...v0.10.0) (2026-11-28)


### ✨ Features

* print the version with vt --version ([ab0b207](https://github.com/will-molloy/video-tools/commit/ab0b207f96f3af3540c4461ad4f61834628d1bd2))


### 📝 Documentation

* backfill changelog from github releases ([eb539b6](https://github.com/will-molloy/video-tools/commit/eb539b6a8afa75993f0ca426720704bfc62e8e1f))


### 👷 CI

* build on macos too ([ab68c0d](https://github.com/will-molloy/video-tools/commit/ab68c0da92bdaf5944e094b88b23bf289be447ce))
* rename conventional-commits workflow to commitlint ([8ab32f7](https://github.com/will-molloy/video-tools/commit/8ab32f7a0f917a995db7083b46d37d8da97bbda2))
* switch releases to release-please ([69c94af](https://github.com/will-molloy/video-tools/commit/69c94afed78538d9b4ca8109776f7c38a0db9e95))
* switch releases to release-please ([#27](https://github.com/will-molloy/video-tools/issues/27)) ([53878b5](https://github.com/will-molloy/video-tools/commit/53878b5364a2426d0c879fcdc63ddab754bc86b2))


### 🔧 Miscellaneous

* use semantic commits for renovate PRs ([c83ea90](https://github.com/will-molloy/video-tools/commit/c83ea90c578bcf1216bac9772f94933832aea3a1))

## [0.9.3](https://github.com/will-molloy/video-tools/compare/v0.9.2...v0.9.3) (2026-11-13)

### 🔧 Miscellaneous

* **deps:** update rust dependencies ([#24](https://github.com/will-molloy/video-tools/pull/24)) ([e7c8e72](https://github.com/will-molloy/video-tools/commit/e7c8e725c90d2d5f817c8e21b81d09a0c0cd74b7))

## [0.9.2](https://github.com/will-molloy/video-tools/compare/v0.9.1...v0.9.2) (2026-11-13)

### 🔧 Miscellaneous

* **deps:** update ci/cd dependencies ([#23](https://github.com/will-molloy/video-tools/pull/23)) ([8f6214c](https://github.com/will-molloy/video-tools/commit/8f6214c1f8c1a317e1e05e57abd323fce66ee8b5))

## [0.9.1](https://github.com/will-molloy/video-tools/compare/v0.9.0...v0.9.1) (2026-11-13)

### 🔧 Miscellaneous

* **deps:** update dependency nltk to v3.10.4 [security] ([#22](https://github.com/will-molloy/video-tools/pull/22)) ([f3c528a](https://github.com/will-molloy/video-tools/commit/f3c528a589c2405ca6f4729eba594d123be21cb4))

## [0.9.0](https://github.com/will-molloy/video-tools/compare/v0.8.0...v0.9.0) (2026-11-13)

### ✨ Features

* add loudness-check tool ([#21](https://github.com/will-molloy/video-tools/pull/21)) ([bf5d686](https://github.com/will-molloy/video-tools/commit/bf5d6867b75e87dcd0c7ccd4121b1cde4cef9a3e))

### ♻️ Refactoring

* share the streaming subprocess loop ([07c7056](https://github.com/will-molloy/video-tools/commit/07c70560d2bbfb815f3b63dd15adc52c97a1cc5e))

## [0.8.0](https://github.com/will-molloy/video-tools/compare/v0.7.0...v0.8.0) (2026-11-02)

### ✨ Features

* support Windows ([#19](https://github.com/will-molloy/video-tools/pull/19)) ([d6b5e62](https://github.com/will-molloy/video-tools/commit/d6b5e6201036faddc06596d6eb11844f2fd3ff9d))
* detect NVIDIA GPUs on Windows ([3fedb59](https://github.com/will-molloy/video-tools/commit/3fedb5995199aa657699a891f3bda572a98b5068))
* handle Windows console interrupts ([65056ed](https://github.com/will-molloy/video-tools/commit/65056edc12c8f9d98bb69d17d8d16e283bdb5313))
* cancel vt-ml by closing its stdin and stop its tree portably ([fb8300b](https://github.com/will-molloy/video-tools/commit/fb8300b756bfed49f62f17a0454a1df31c284c3f))

### 📝 Documentation

* mark the ML tools as experimental on Windows ([f444927](https://github.com/will-molloy/video-tools/commit/f444927a687401034b56d606cad3d527dad9281f))

### ✅ Tests

* build cache paths and read ffprobe output portably ([4069a0a](https://github.com/will-molloy/video-tools/commit/4069a0ae2fa74e7eec75bf5f8e16002334f0c36b))

### 👷 CI

* drop the aggregate build job ([940afa2](https://github.com/will-molloy/video-tools/commit/940afa29cce6a8b46baa8f78089d9e64fc23cc58))
* run the build on Windows ([46c0614](https://github.com/will-molloy/video-tools/commit/46c0614feabec08b3532910175056f42d95c6762))

## [0.7.0](https://github.com/will-molloy/video-tools/compare/v0.6.1...v0.7.0) (2026-11-02)

### ✨ Features

* run tools on multiple files ([#18](https://github.com/will-molloy/video-tools/pull/18)) ([562df13](https://github.com/will-molloy/video-tools/commit/562df13cbfd20ee2f252e66e6727dee7eadd2774))
* show each file's result in the TUI as it finishes ([39fcc8e](https://github.com/will-molloy/video-tools/commit/39fcc8e5ebd66c9bad0a9635e1ff2b071768633b))
* show Dialogue Separation's options only once it is picked ([febdc8e](https://github.com/will-molloy/video-tools/commit/febdc8eccfd1d8ae9d0a1025481780285f622a19))
* set Dialogue Separation's options on the picker screen ([1a1c5e5](https://github.com/will-molloy/video-tools/commit/1a1c5e51c91c8b37888cb1ac314b12112dbea0ce))
* scale parallel ffprobe tools with the CPU thread count ([ce27b3a](https://github.com/will-molloy/video-tools/commit/ce27b3a9f6aa9210ceb1e4bd66f690c757976cbc))
* run a tool on multiple files from one TUI screen ([9e08f19](https://github.com/will-molloy/video-tools/commit/9e08f19ad5e47f907ddcea94767ec65bcbf9037d))
* run each tool on multiple files from the CLI ([4c5ee5c](https://github.com/will-molloy/video-tools/commit/4c5ee5c4e96d3d1ce0dddd54a52485615d6177b9))

### 📝 Documentation

* say ML tools run one at a time because of the GPU ([76fdce9](https://github.com/will-molloy/video-tools/commit/76fdce936e9d0379bb9f124020fba8d1b2185796))
* note ML models download on first run ([ce659df](https://github.com/will-molloy/video-tools/commit/ce659dfecf6837187fdebf8375a922edf71a0557))
* list VRAM and disk for each ML model ([6ff758b](https://github.com/will-molloy/video-tools/commit/6ff758b2d9701a5c1487c18011caaf57151782e5))

## [0.6.1](https://github.com/will-molloy/video-tools/compare/v0.6.0...v0.6.1) (2026-11-02)

### ♻️ Refactoring

* rename stage to subcommand and quote to display ([#14](https://github.com/will-molloy/video-tools/pull/14)) ([9c25e6b](https://github.com/will-molloy/video-tools/commit/9c25e6b02181ad67614ecbc2711b479890e3273f))
* name vt-ml's subcommands and the path display plainly ([857e8b9](https://github.com/will-molloy/video-tools/commit/857e8b983210edb0c38c9f38182ed856ebf6a75b))

## [0.6.0](https://github.com/will-molloy/video-tools/compare/v0.5.0...v0.6.0) (2026-11-02)

### ✨ Features

* add dialogue-separation tool ([#9](https://github.com/will-molloy/video-tools/pull/9)) ([97af825](https://github.com/will-molloy/video-tools/commit/97af82539a334437e8f3e9d8022df1d7a405be34))
* quote paths in separation messages ([e59a24c](https://github.com/will-molloy/video-tools/commit/e59a24c86b53b9819193ca56cdfd5544d2f796c7))
* offer dialogue separation as one TUI tool ([b030686](https://github.com/will-molloy/video-tools/commit/b030686d176fa5d260ded1beba30369378cf250b))
* keep an existing separation output unless forced ([fd9b885](https://github.com/will-molloy/video-tools/commit/fd9b8858d35ce118867ab72c2d1fa6b1868546c9))
* separate only the speech in the transcript ([2e5b389](https://github.com/will-molloy/video-tools/commit/2e5b389b07d813e10bdd0438d0ad6a981eab4bbd))
* read SRT transcripts ([a53a67c](https://github.com/will-molloy/video-tools/commit/a53a67c22815de724aa22ad5069c576fc090ea28))
* add the separate ML stage ([088eca7](https://github.com/will-molloy/video-tools/commit/088eca726e1c8c11064b1055284598b5074c4daf))

## [0.5.0](https://github.com/will-molloy/video-tools/compare/v0.4.1...v0.5.0) (2026-11-02)

### ✨ Features

* add transcribe tool ([#11](https://github.com/will-molloy/video-tools/pull/11)) ([7a8a5b8](https://github.com/will-molloy/video-tools/commit/7a8a5b8ae10307cb545b0df76104972651acddf1))
* put the transcription time on its own line ([b5f5ad6](https://github.com/will-molloy/video-tools/commit/b5f5ad624035e6b3096c904fe9abfbaba9bdb893))
* keep file names in the default colour ([f8b695b](https://github.com/will-molloy/video-tools/commit/f8b695b3f02e041a256016f060b18288928088fe))
* show file names in bold in the TUI ([2fd7357](https://github.com/will-molloy/video-tools/commit/2fd735743aa54dda66bc7602d0ac935be9c9516f))
* quote paths in messages ([19c8b36](https://github.com/will-molloy/video-tools/commit/19c8b36655d4fdffb4ea23f56459e4e1e7d0711a))
* confirm before a tool replaces its output ([7c6d42a](https://github.com/will-molloy/video-tools/commit/7c6d42ad811c1ba921b5925633cd30699b75977e))
* add vt transcribe ([ec91d0c](https://github.com/will-molloy/video-tools/commit/ec91d0c9e1ec109a763bf0d7e906ef58ed4c6b04))
* add the transcribe ML stage ([9941d75](https://github.com/will-molloy/video-tools/commit/9941d75220de47922c6ba05b3249ddaf82c663f7))

### 🐛 Bug Fixes

* show a stage's failure one line at a time ([f338379](https://github.com/will-molloy/video-tools/commit/f338379fe33f7571cf27071a92a71530bfeb177b))
* keep Whisper's memory flat on long files ([49d5e68](https://github.com/will-molloy/video-tools/commit/49d5e68a2b619ea44604478bcbb6769a4eabe9b8))

### 📝 Documentation

* note Whisper downloads on first run ([1951e53](https://github.com/will-molloy/video-tools/commit/1951e53902342d527dcacbdc49d57ee65a0825c2))
* say tools run directly from the CLI ([6eab9ff](https://github.com/will-molloy/video-tools/commit/6eab9ff7b2d2b7ad91486a1978931eeebf612bfa))
* list each tool's requirements as bullets ([bbbea24](https://github.com/will-molloy/video-tools/commit/bbbea24183cdd0cd737650c53ad9bcf757aceafa))
* list the tools in a README table ([290d93a](https://github.com/will-molloy/video-tools/commit/290d93afa249ccc210088851953e06c11cf7f007))
* describe vt transcribe as extracting dialogue ([69e8ab4](https://github.com/will-molloy/video-tools/commit/69e8ab47343526b2892d5b3d7cff6ab249ffff8c))

### ♻️ Refactoring

* use each tool's summary as its CLI help ([27f8b38](https://github.com/will-molloy/video-tools/commit/27f8b38fb221e397b3726103178435317acea943))

## [0.4.1](https://github.com/will-molloy/video-tools/compare/v0.4.0...v0.4.1) (2026-10-20)

### 📝 Documentation

* add contributor rules ([#13](https://github.com/will-molloy/video-tools/pull/13)) ([698e883](https://github.com/will-molloy/video-tools/commit/698e8830b98a631ec9b30d6c0fd4af452fb1a994))
* link CONTRIBUTING.md from the README ([a12cd58](https://github.com/will-molloy/video-tools/commit/a12cd587b24bf8381a8aecf5194b726872bcef76))
* move the development notes to CONTRIBUTING.md ([cfd5723](https://github.com/will-molloy/video-tools/commit/cfd5723d2ac535507ac749ee940874128bfd1674))

## [0.4.0](https://github.com/will-molloy/video-tools/compare/v0.3.2...v0.4.0) (2026-10-19)

### ✨ Features

* add base ML project ([#10](https://github.com/will-molloy/video-tools/pull/10)) ([cf7f8ad](https://github.com/will-molloy/video-tools/commit/cf7f8ad11b5054203cc7ea779773cb9fa84a1dcc))
* add the Rust side of running ML stages ([0c2aa3b](https://github.com/will-molloy/video-tools/commit/0c2aa3b9031089cb4dfddb362ae9055350010088))
* add the vt-ml Python project for ML stages ([21f40cc](https://github.com/will-molloy/video-tools/commit/21f40cc65f4500df4fbb2b0e2364549aefbf325f))

### ♻️ Refactoring

* trim the ML helpers to what their first users need ([092b107](https://github.com/will-molloy/video-tools/commit/092b107e1c9aec1202de7310223e9ec347e5db55))

### 📦 Build

* pin vt-ml to Python 3.12.15 ([4b95247](https://github.com/will-molloy/video-tools/commit/4b95247a957edef5789ca1e8194671c91c213e54))

### 👷 CI

* split the build into build-ml and build-tui actions ([381ac99](https://github.com/will-molloy/video-tools/commit/381ac99e454d982fec0caec82539de306f5ff8ca))

## [0.3.2](https://github.com/will-molloy/video-tools/compare/v0.3.1...v0.3.2) (2026-10-19)

### 👷 CI

* add Codecov coverage reporting ([#8](https://github.com/will-molloy/video-tools/pull/8)) ([bbc7703](https://github.com/will-molloy/video-tools/commit/bbc7703f026b1b149c0096bb99ee1b8ab6148a07))
* group test setup into a setup-test action ([ee64696](https://github.com/will-molloy/video-tools/commit/ee646965b13fbfa42db0e87b529b0317c96f490f))

## [0.3.1](https://github.com/will-molloy/video-tools/compare/v0.3.0...v0.3.1) (2026-10-19)

### 👷 CI

* tag the commit each release run built ([#7](https://github.com/will-molloy/video-tools/pull/7)) ([7fffd05](https://github.com/will-molloy/video-tools/commit/7fffd05a0543a876843a764b4827c0929dce675c))

## [0.3.0](https://github.com/will-molloy/video-tools/compare/v0.2.0...v0.3.0) (2026-10-19)

### ✨ Features

* add sync-check tool ([#4](https://github.com/will-molloy/video-tools/pull/4)) ([68ad972](https://github.com/will-molloy/video-tools/commit/68ad97263adf513641bf6f43f2281ad494126468))
* drop sync-check JSON output ([c33c640](https://github.com/will-molloy/video-tools/commit/c33c640f02f963646c621ca29e7eec409df11993))
* scan whole files and skip costly fixes for imperceptible desync ([c189d0f](https://github.com/will-molloy/video-tools/commit/c189d0fd045a9844f81da037bb6d0964ec2f1d63))
* run sync check from the TUI ([32c02ec](https://github.com/will-molloy/video-tools/commit/32c02ec875d6f2782bcad26dc8bd123f76febd13))
* add sync-check command ([c5d3de8](https://github.com/will-molloy/video-tools/commit/c5d3de8a25a6a7669be10ef028815009b2118f0e))

### 📝 Documentation

* drop sync-check notes from README ([df7417a](https://github.com/will-molloy/video-tools/commit/df7417abfa098dae41496a139a85c648a6625697))
* align README usage comments ([e76894e](https://github.com/will-molloy/video-tools/commit/e76894ed63da56412973f5a89fdab3e6f816ccf6))

### ♻️ Refactoring

* make sync-check report internals private ([78a8686](https://github.com/will-molloy/video-tools/commit/78a868636b18f7a2f108c0c6d346c58bd3bd2e74))
* move chapters tool into its own directory ([7578ffe](https://github.com/will-molloy/video-tools/commit/7578ffed2773e82221b27f2e444e09780c2d9a5a))

### ✅ Tests

* add chapters fixtures and snapshot tests ([#5](https://github.com/will-molloy/video-tools/pull/5)) ([94946f5](https://github.com/will-molloy/video-tools/commit/94946f51143f37b925891f2b5d624ca038ed5a6b))
* share the vt runner across CLI tests ([055caa1](https://github.com/will-molloy/video-tools/commit/055caa1bf4d6fc20db0bc2c8cbf5ecb7bc1ecc45))
* drop TUI snapshot tests ([0776c59](https://github.com/will-molloy/video-tools/commit/0776c59c09aff3327e490b10526a05c759ff50ee))
* add sync-check fixtures and snapshot tests ([c86b5ad](https://github.com/will-molloy/video-tools/commit/c86b5ad90e9a9005600b7ae96724cf94e342a483))

## [0.2.0](https://github.com/will-molloy/video-tools/compare/v0.1.0...v0.2.0) (2026-10-19)

### ✨ Features

* add chapters tool ([#2](https://github.com/will-molloy/video-tools/pull/2)) ([3e85b96](https://github.com/will-molloy/video-tools/commit/3e85b96054f397add97bc0391746b32997256e49))
* run chapters tool from the TUI ([4d08f25](https://github.com/will-molloy/video-tools/commit/4d08f2599e0c0d1d938746a96bb2da2bb18db90d))
* add chapters command ([01d59fe](https://github.com/will-molloy/video-tools/commit/01d59fe667564a17e33f6c56ab5e5d7b3d105575))

## [0.1.0](https://github.com/will-molloy/video-tools/releases/tag/v0.1.0) (2026-10-19)

* Initial release
