# Release public keys

Minisign public keys that sign Vibeke releases. Both are embedded in the binary (`crates/vk-remote/src/bootstrap.rs`) and in `scripts/install.sh`. See [docs/releases.md](../docs/releases.md) for verification and key rotation.

| File | Label | Key id (fingerprint) | Public key |
| --- | --- | --- | --- |
| `vibeke-2026.pub` | current | `5F6E09C78F555F34` | `RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z` |
| `vibeke-next.pub` | next | `69536A23D04E2C7C` | `RWR8LE7QI2pTaSsb4srEFbF1j78fXZzbORy4KGRzHErddJwSJLxwqH3x` |

Secret keys are never stored in this repository.
