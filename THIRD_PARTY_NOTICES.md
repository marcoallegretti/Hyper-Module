# Third-Party Notices

## Symphonia 0.5.5

The `hyper-audio-source` crate uses selected components of Symphonia 0.5.5:

- `symphonia`
- `symphonia-core`
- `symphonia-metadata`
- `symphonia-bundle-flac`
- `symphonia-codec-pcm`
- `symphonia-format-riff`

Symphonia is Copyright © 2019–2022 The Project Symphonia Developers and is
licensed under the Mozilla Public License 2.0.

Source: <https://github.com/pdeljanov/Symphonia/tree/6d533f26150953a882a6a111ebd13f0abf7129d5>

License: <https://www.mozilla.org/MPL/2.0/>

## music-metadata FLAC test vector

The FLAC conformance vector embedded in
`crates/audio-source/tests/decode.rs` is derived from
`test/samples/flac/libFLAC.flac` in music-metadata 11.14.0, commit
`959f0f3d464578ddd3dfc2a5cbcd0068799dd394`.

Copyright © 2026 Borewit. Used under the MIT License.

Source: <https://github.com/Borewit/music-metadata/blob/959f0f3d464578ddd3dfc2a5cbcd0068799dd394/test/samples/flac/libFLAC.flac>

License: <https://opensource.org/license/mit>

## Serde 1.0.229 and serde_json 1.0.151

The `hyper-playback-json` crate uses:

- `serde`, `serde_core`, and `serde_derive` 1.0.229;
- `serde_json` 1.0.151;
- the transitive packages `itoa` 1.0.18, `memchr` 2.8.3, `proc-macro2`
  1.0.107, `quote` 1.0.47, `syn` 3.0.3, `unicode-ident` 1.0.24, and `zmij`
  1.0.23.

Serde, serde_json, itoa, proc-macro2, quote, syn, and the Rust portions of
unicode-ident are available under the MIT License or Apache License 2.0.
`memchr` is available under the Unlicense or MIT License. `zmij` is available
under the MIT License. `unicode-ident` also incorporates Unicode data under the
Unicode License 3.0.

Sources:

- <https://github.com/serde-rs/serde/tree/v1.0.229>
- <https://github.com/serde-rs/json/tree/v1.0.151>
- <https://github.com/dtolnay/itoa>
- <https://github.com/BurntSushi/memchr>
- <https://github.com/dtolnay/proc-macro2>
- <https://github.com/dtolnay/quote>
- <https://github.com/dtolnay/syn>
- <https://github.com/dtolnay/unicode-ident>
- <https://github.com/dtolnay/zmij>

## rtrb 0.3.4

The `hyper-mixer-runtime` crate uses `rtrb` 0.3.4 for bounded
single-producer/single-consumer transport command delivery.

The crate metadata lists Stjepan Glavina and Matthias Geier as authors.

Licensed under either the MIT License or Apache License 2.0.

Source: <https://github.com/mgeier/rtrb/tree/0.3.4>

Licenses:

- <https://github.com/mgeier/rtrb/blob/0.3.4/LICENSE-MIT>
- <https://github.com/mgeier/rtrb/blob/0.3.4/LICENSE-APACHE>

## ringbuf 0.5.1

The `hyper-recording-transport` crate uses `ringbuf` 0.5.1 and its
`crossbeam-utils` 0.8.22 dependency for bounded single-producer/single-consumer
audio transfer.

Both packages are licensed under either the MIT License or Apache License 2.0.

Sources:

- <https://github.com/agerasev/ringbuf/tree/f76754f1ce15790406358bef3dcf231c400bcfd8>
- <https://github.com/crossbeam-rs/crossbeam/tree/crossbeam-utils-0.8.22>

## Hound 3.5.1, crc32fast 1.5.0, and sha2 0.10.9

The `hyper-recording-journal` crate uses Hound 3.5.1 to write and independently
read 32-bit IEEE-float WAV media. Hound is licensed under Apache License 2.0.

The crate uses `crc32fast` 1.5.0 and its `cfg-if` 1.0.4 dependency to verify
journal headers, chunks, and finalized sample content. Both packages are
licensed under either the MIT License or Apache License 2.0.

The crate uses RustCrypto `sha2` 0.10.9 and the locked `digest` 0.10.7,
`block-buffer` 0.10.4, `crypto-common` 0.1.7, `generic-array` 0.14.7,
`typenum` 1.20.1, `cpufeatures` 0.2.17, and `version_check` 0.9.5 packages to
issue project-compatible SHA-256 descriptors for verified WAV files. These
packages are licensed under either the MIT License or Apache License 2.0.

Sources:

- <https://github.com/ruuda/hound/tree/v3.5.1>
- <https://github.com/srijs/rust-crc32fast/tree/v1.5.0>
- <https://github.com/rust-lang/cfg-if/tree/1.0.4>
- <https://github.com/RustCrypto/hashes/tree/sha2-v0.10.9>
- <https://github.com/RustCrypto/traits/tree/digest-v0.10.7>
- <https://github.com/RustCrypto/utils/tree/block-buffer-v0.10.4>
- <https://github.com/RustCrypto/traits/tree/crypto-common-v0.1.7>
- <https://github.com/fizyk20/generic-array/tree/0.14.7>
- <https://github.com/paholg/typenum/tree/v1.20.1>
- <https://github.com/RustCrypto/utils/tree/cpufeatures-v0.2.17>
- <https://github.com/SergioBenitez/version_check/tree/v0.9.5>

## PipeWire Rust bindings 0.10.0

The Linux portion of `hyper-desktop-audio` uses the `pipewire`, `libspa`,
`pipewire-sys`, and `libspa-sys` 0.10.0 crates.

Copyright the pipewire-rs contributors. Licensed under the MIT License.

Source: <https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/tree/v0.10.0>

License: <https://gitlab.freedesktop.org/pipewire/pipewire-rs/-/blob/v0.10.0/LICENSE>

## bytemuck 1.24.0

The Linux PipeWire and Windows WASAPI callbacks use `bytemuck` 1.24.0 for
checked conversion between byte buffers and native `f32` samples without
unsafe code in this repository.

Copyright the bytemuck contributors. Licensed under the zlib License,
Apache License 2.0, or MIT License.

Source: <https://github.com/Lokathor/bytemuck/tree/da748163ea203f80098b6bdc754c54ebc535364c>

Licenses:

- <https://github.com/Lokathor/bytemuck/blob/da748163ea203f80098b6bdc754c54ebc535364c/LICENSE-ZLIB>
- <https://github.com/Lokathor/bytemuck/blob/da748163ea203f80098b6bdc754c54ebc535364c/LICENSE-APACHE>
- <https://github.com/Lokathor/bytemuck/blob/da748163ea203f80098b6bdc754c54ebc535364c/LICENSE-MIT>

## wasapi 0.22.0

The Windows portion of `hyper-desktop-audio` uses the `wasapi` 0.22.0 crate
and its Microsoft `windows-rs` dependencies for event-driven shared-mode
audio rendering.

Copyright the wasapi-rs contributors. Licensed under the MIT License.

Source: <https://github.com/HEnquist/wasapi-rs/tree/v0.22.0>

License: <https://github.com/HEnquist/wasapi-rs/blob/v0.22.0/LICENSE.txt>

The `windows-rs` dependencies are Copyright Microsoft Corporation and
licensed under the MIT License or Apache License 2.0.

Source: <https://github.com/microsoft/windows-rs>

Licenses:

- <https://github.com/microsoft/windows-rs/blob/master/license-mit>
- <https://github.com/microsoft/windows-rs/blob/master/license-apache>
