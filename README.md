# tokio-curl

An implementation of an asynchronous HTTP client using futures backed by
libcurl.

[![Build Status](https://travis-ci.org/tokio-rs/tokio-curl.svg?branch=master)](https://travis-ci.org/tokio-rs/tokio-curl)
[![Build status](https://ci.appveyor.com/api/projects/status/1uqcw7g5e5ah3or2?svg=true)](https://ci.appveyor.com/project/alexcrichton/tokio-curl)

[Documentation](https://docs.rs/tokio-curl)

## Usage

First, add this to your `Cargo.toml`:

```toml
[dependencies]
tokio-curl = "0.1"
```

Next, add this to your crate:

```rust
extern crate tokio_curl;
```

# License

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Serde by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
