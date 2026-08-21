# Third-party algorithm references

The osu!mania analyzer is an original clean-room Rust implementation. It does not embed or execute the JavaScript, generated meta models, WASM binaries, Azusa, Daniel, Sunny, Roxy calibration head, or MinaCalc from the projects below.

Its public design references the documented ideas in:

- **osumania_map_analyser**, commit `f146c479fde24523b4d0909b83f8008b9d6815b2`, by Leo_Black.
  - Roxy structural strain documentation: <https://github.com/LeoBlackMT/osumania_map_analyser/blob/f146c479fde24523b4d0909b83f8008b9d6815b2/docs/roxy_algorithm.md>
  - Pattern-analysis documentation: <https://github.com/LeoBlackMT/osumania_map_analyser/blob/f146c479fde24523b4d0909b83f8008b9d6815b2/docs/features/pattern-analysis.md>
  - License: MIT, copyright (c) 2026 Leo_Black. The full upstream license is available at <https://github.com/LeoBlackMT/osumania_map_analyser/blob/f146c479fde24523b4d0909b83f8008b9d6815b2/LICENSE>.

The implementation uses the general concepts of bounded per-row signals, burst/sustain decay, tail-aware aggregation, and continuous pattern composition. Constants, multi-key generalization, LN handling, persistence, normalization, bucketing, and similarity distance are specific to this project.
