# Wisp local patches

This directory is the published `wasapi` 0.23.0 crate (crates.io checksum
`80c3aa5d6b0e7acc3ea10cb19c334df0c8d825060f14a30d9e3b03385e6e5175`)
with two focused changes in `src/api.rs`:

- never dereference the nullable WASAPI data pointer for
  `AUDCLNT_BUFFERFLAGS_SILENT`, while still releasing every acquired buffer;
- distinguish a normal `WAIT_TIMEOUT` from `WaitForSingleObject` failures.

The accompanying unit tests cover silent/null payload handling and wait-result
classification. Remove the workspace patch when an upstream release contains
equivalent fixes.
