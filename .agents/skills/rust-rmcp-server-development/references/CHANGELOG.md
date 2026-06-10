# RMCP Changelog: 1.5.0 Through 1.7.0

This file captures the `rmcp` changelog sections from 1.5.0 through the current release verified on 2026-06-09 (1.7.0 still latest).

Source: `crates/rmcp/CHANGELOG.md` in `modelcontextprotocol/rust-sdk`

Source URL: https://github.com/modelcontextprotocol/rust-sdk/blob/main/crates/rmcp/CHANGELOG.md

Source license note: the MCP project license file states that documentation contributions, excluding specifications, are licensed under CC-BY-4.0, with project code under Apache-2.0 / MIT transition terms. Retain this attribution when reusing this captured changelog.

## [1.7.0](https://github.com/modelcontextprotocol/rust-sdk/compare/rmcp-v1.6.0...rmcp-v1.7.0) - 2026-05-13

### Added

- add task-based stdio examples ([#839](https://github.com/modelcontextprotocol/rust-sdk/pull/839))

### Fixed

- *(rmcp)* flatten Resource variant of PromptMessageContent ([#843](https://github.com/modelcontextprotocol/rust-sdk/pull/843))
- reply -32700 on stdio parse errors instead of closing ([#833](https://github.com/modelcontextprotocol/rust-sdk/pull/833))

### Other

- *(rmcp)* remove dependency on chrono default features ([#829](https://github.com/modelcontextprotocol/rust-sdk/pull/829))
- Fix/issue 817 idle timeout log level ([#824](https://github.com/modelcontextprotocol/rust-sdk/pull/824))

## [1.6.0](https://github.com/modelcontextprotocol/rust-sdk/compare/rmcp-v1.5.0...rmcp-v1.6.0) - 2026-05-01

### Added

- *(http)* log Host/Origin rejections ([#826](https://github.com/modelcontextprotocol/rust-sdk/pull/826))
- *(http)* add Origin header validation ([#823](https://github.com/modelcontextprotocol/rust-sdk/pull/823))
- *(router)* support runtime disabling of tools ([#809](https://github.com/modelcontextprotocol/rust-sdk/pull/809))
- optional session store (resumabillity support) ([#775](https://github.com/modelcontextprotocol/rust-sdk/pull/775))

### Fixed

- add init_timeout for streamable-http sessions ([#811](https://github.com/modelcontextprotocol/rust-sdk/pull/811))
- *(http)* fall back to :authority for HTTP/2 ([#827](https://github.com/modelcontextprotocol/rust-sdk/pull/827))
- *(docs)* use correct Parameters<T> syntax in tool examples ([#814](https://github.com/modelcontextprotocol/rust-sdk/pull/814))

### Other

- add systemprompt-template to Built with rmcp ([#820](https://github.com/modelcontextprotocol/rust-sdk/pull/820))

## [1.5.0](https://github.com/modelcontextprotocol/rust-sdk/compare/rmcp-v1.4.0...rmcp-v1.5.0) - 2026-04-16

### Added

- *(transport)* add constructors for non_exhaustive error types ([#806](https://github.com/modelcontextprotocol/rust-sdk/pull/806))
- add 2025-11-25 protocol version support ([#802](https://github.com/modelcontextprotocol/rust-sdk/pull/802))

### Fixed

- treat resource metadata JSON parse failure as soft error ([#810](https://github.com/modelcontextprotocol/rust-sdk/pull/810))
- include http_request_id in request-wise priming event IDs ([#799](https://github.com/modelcontextprotocol/rust-sdk/pull/799))
- *(http)* drain SSE stream for connection reuse ([#790](https://github.com/modelcontextprotocol/rust-sdk/pull/790))

### Other

- *(deps)* update which requirement from 7 to 8 ([#807](https://github.com/modelcontextprotocol/rust-sdk/pull/807))
