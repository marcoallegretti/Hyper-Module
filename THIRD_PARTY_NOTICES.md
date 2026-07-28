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
