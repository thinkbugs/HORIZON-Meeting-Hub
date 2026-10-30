# Third-Party Notices

HORIZON Meeting includes the following open-source software.

## Drawnix

- Package: `@drawnix/drawnix` 0.4.0-0
- Source: https://github.com/plait-board/drawnix
- License: MIT
- Copyright (c) 2024 Drawnix

## Plait

- Packages: `@plait/*` 0.89.2, `@plait-board/react-board`, `@plait-board/react-text`
- Source: https://github.com/worktile/plait
- License: MIT
- Copyright (c) 2018 - 2023 Worktile Inc. https://worktile.com

## Airplayer

- Source: https://github.com/joyjoyfresh/Airplayer
- License: MIT (see the upstream LICENSE for its own third-party components)
- Copyright (c) 2026 AirPlayer

Used as a protocol reference for the AirPlay mirroring handshake. The Rust
receiver in `crates/horizon-cast` is an independent implementation; the fixed
FPLY reply bytes and the feature bitmap in `airplay/fp_replies.rs` and
`airplay/fairplay_tables.rs` are protocol constants that every AirPlay receiver
carries, transcribed from that reference.

## MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

Other third-party packages are used under the licenses declared in their own
package metadata; see `package-lock.json` for the complete dependency tree.
