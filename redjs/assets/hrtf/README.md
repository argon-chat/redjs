# HRTF impulse responses

`Composite.flac` is vendored verbatim from Chromium
(`third_party/blink/renderer/platform/audio/resources/Composite.flac`,
BSD-3-Clause, Copyright The Chromium Authors) — the exact HRIR set Chrome uses
for Web Audio `PannerNode` `panningModel: 'HRTF'`. IRCAM-Listen-derived
composite: 240 responses (24 azimuths × 10 elevations) × 256 frames, stereo,
44.1 kHz. Layout/indexing documented in `src/webapi/hrtf.rs`.
