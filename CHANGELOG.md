# Changelog

## [0.3.0+qn](https://github.com/quiknode-labs/shredstream-proxy/compare/v0.2.14+qn...v0.3.0+qn) (2026-06-12)


### Features

* measure first-seen vs duplicate-arrival lag per source pair (dup_lag) ([eda6120](https://github.com/quiknode-labs/shredstream-proxy/commit/eda61208228e2b227327024d64f1e4409b8eb31f))
* measure first-seen vs duplicate-arrival lag per source pair (dup_lag) ([27ddabf](https://github.com/quiknode-labs/shredstream-proxy/commit/27ddabf26a7ec1f157a80501b28923a25fcacc53))


### Bug Fixes

* harden dup_lag rotation against idle gaps and concurrent first-seen ([47a51b2](https://github.com/quiknode-labs/shredstream-proxy/commit/47a51b2eecffb4640cbee78c76ea80feca8fe352))
* make dup_lag shard flip exclusive against observers ([d9e75a6](https://github.com/quiknode-labs/shredstream-proxy/commit/d9e75a6747ab75c0807667c324b4059af1e23d07))
* rotate dup_lag shards on every arrival, not only sampled ones ([e7f3945](https://github.com/quiknode-labs/shredstream-proxy/commit/e7f394546c4f08c7fb6c89842e4c97e628a7014a))

## [0.2.14+qn](https://github.com/quiknode-labs/shredstream-proxy/compare/v0.2.13+qn...v0.2.14+qn) (2026-05-29)


### Bug Fixes

* scope heartbeat loop / grpc client to the shredstream client loop ([#36](https://github.com/quiknode-labs/shredstream-proxy/issues/36)) ([efbf5ce](https://github.com/quiknode-labs/shredstream-proxy/commit/efbf5ce05aa4d5917a5723b93b102331845ee51d))
* switch release-please to simple type (workspace inheritance) ([ac8d276](https://github.com/quiknode-labs/shredstream-proxy/commit/ac8d276b1566ae2a865313f13a57864cf7c0aedd))
* switch release-please to simple type to support workspace inheritance ([472da2e](https://github.com/quiknode-labs/shredstream-proxy/commit/472da2e8be02c83d42c2b8debe2335aa802b2992))


### Continuous Integration

* add release-please versioning pipeline ([cdc3309](https://github.com/quiknode-labs/shredstream-proxy/commit/cdc33092401cc11543c74e10ffe1ae8d68f7e28d))
