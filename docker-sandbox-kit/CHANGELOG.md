# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0](https://github.com/aartintelligent/docker-sandbox/compare/docker-sandbox-kit-v0.3.1...docker-sandbox-kit-v0.4.0) - 2026-11-28

### Added

- *(blueprint)* launch a sandbox from its spec and own it ([#68](https://github.com/aartintelligent/docker-sandbox/pull/68))
- *(kit)* check a composition before anything is pulled ([#65](https://github.com/aartintelligent/docker-sandbox/pull/65))
- *(kit)* write agents, descriptors and mixins as json ([#64](https://github.com/aartintelligent/docker-sandbox/pull/64))
- *(kit)* [**breaking**] register mcp servers in copilot and opencode, and stdio ones in the sandbox ([#47](https://github.com/aartintelligent/docker-sandbox/pull/47))
- *(kit)* keep the package of each composition in a cache ([#61](https://github.com/aartintelligent/docker-sandbox/pull/61))
- *(kit)* read a kit from its name as the cli does ([#59](https://github.com/aartintelligent/docker-sandbox/pull/59))
- *(kit)* write a mixin as a kit directory the cli reads ([#58](https://github.com/aartintelligent/docker-sandbox/pull/58))
- *(sandbox)* say whether a kit package suits a daemon ([#53](https://github.com/aartintelligent/docker-sandbox/pull/53))

### Fixed

- *(kit)* take an argument named by its kit, as the cli does ([#57](https://github.com/aartintelligent/docker-sandbox/pull/57))
- *(kit)* resolve an agent to its shortcut and name its base as the cli does ([#51](https://github.com/aartintelligent/docker-sandbox/pull/51))
- *(sandbox)* stage kit packages under a name no other save shares ([#48](https://github.com/aartintelligent/docker-sandbox/pull/48))

## [0.3.1](https://github.com/aartintelligent/docker-sandbox/compare/docker-sandbox-kit-v0.3.0...docker-sandbox-kit-v0.3.1) - 2026-11-28

### Fixed

- *(kit)* leave out the open-url capability the claude kit now declares ([#45](https://github.com/aartintelligent/docker-sandbox/pull/45))
