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

`tokio-curl` is primarily distributed under the terms of both the MIT
license and the Apache License (Version 2.0), with portions covered by various
BSD-like licenses.

See LICENSE-APACHE, and LICENSE-MIT for details.
