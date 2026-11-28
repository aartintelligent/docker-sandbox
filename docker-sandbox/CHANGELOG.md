# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.2](https://github.com/aartintelligent/docker-sandbox/compare/v0.3.1...v0.3.2) - 2026-11-28

### Fixed

- *(sandbox)* stage kit packages under a name no other save shares ([#48](https://github.com/aartintelligent/docker-sandbox/pull/48))

## [0.3.0](https://github.com/aartintelligent/docker-sandbox/compare/v0.2.0...v0.3.0) - 2026-11-27

### Added

- *(sandbox)* [**breaking**] hold kit packages in memory and save them when they are to be kept ([#38](https://github.com/aartintelligent/docker-sandbox/pull/38))
- *(sandbox)* name agents and find the hosts of their keys in their kits ([#37](https://github.com/aartintelligent/docker-sandbox/pull/37))
- *(sandbox)* [**breaking**] check the hosts of network rules as the cli checks them ([#35](https://github.com/aartintelligent/docker-sandbox/pull/35))
- *(audit)* measure how the proxy applies the network rules of a sandbox ([#34](https://github.com/aartintelligent/docker-sandbox/pull/34))
- *(sandbox)* stream image exports and build agent creation requests ([#29](https://github.com/aartintelligent/docker-sandbox/pull/29))

### Changed

- *(sandbox)* [**breaking**] move kit forging and compiling into the docker-sandbox-kit crate ([#30](https://github.com/aartintelligent/docker-sandbox/pull/30))

## [0.2.0](https://github.com/aartintelligent/docker-sandbox/compare/v0.1.1...v0.2.0) - 2026-11-27

### Added

- *(sandbox)* give agents their api key through the proxy alone ([#20](https://github.com/aartintelligent/docker-sandbox/pull/20))
- *(sandbox)* rebuild base kits from their image without the sbx cli ([#16](https://github.com/aartintelligent/docker-sandbox/pull/16))
- *(sandbox)* forge kit packages without the sbx cli ([#15](https://github.com/aartintelligent/docker-sandbox/pull/15))
- *(sandbox)* [**breaking**] report withheld credentials and wipe secrets from memory ([#14](https://github.com/aartintelligent/docker-sandbox/pull/14))

### Changed

- *(sandbox)* type the descriptors and artifacts kit forging writes ([#17](https://github.com/aartintelligent/docker-sandbox/pull/17))

### Documentation

- *(sandbox)* present the kits forged without the sbx cli ([#24](https://github.com/aartintelligent/docker-sandbox/pull/24))

### Fixed

- *(sandbox)* order effective capabilities as the cli for kit credentials ([#21](https://github.com/aartintelligent/docker-sandbox/pull/21))

## [0.1.1](https://github.com/aartintelligent/docker-sandbox/compare/v0.1.0...v0.1.1) - 2026-11-25

### Documentation

- *(workspace)* name the repository docker-sandbox in the docs ([#9](https://github.com/aartintelligent/docker-sandbox/pull/9))

## [0.1.0](https://github.com/aartintelligent/docker-sandbox/releases/tag/v0.1.0) - 2026-11-24

### Added

- *(sandbox)* add the typed client of the sandboxd socket
