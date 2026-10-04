# Security: encryption at rest, keys, audit trail

genome-cli stores personal genomic data. By default **everything it writes
under `data_dir` is encrypted**: the kit database, every per-kit genotype
store and the audit log. No plaintext personal data reaches disk unless you
explicitly ask for it: `--insecure-plaintext`, or a plaintext `--output` /
`decrypt` file (which prints a warning).

## 1. Does fsqlite encrypt? (fact check, fsqlite 0.4.9)

frankensqlite.com documents page-level encryption via `PRAGMA fsqlite.key`
(XChaCha20-Poly1305, Argon2id, DEK/KEK envelope). The upstream README says the
cipher exists in `fsqlite-pager` but no `PRAGMA key`/`rekey` dispatch is wired
into `Connection`, and unknown PRAGMAs are silently ignored.

Checked against the crate this repo pins (fsqlite 0.4.9, `Cargo.lock`):

- `fsqlite-pager-0.4.9/src/encrypt.rs` exists (`PageEncryptor`, `KeyManager`)
  behind the pager's `encryption` feature, which the `native` feature turns on;
- nothing in `fsqlite-core` references it, and no `key`/`rekey` pragma is
  handled (`execute_pragma` ignores unknown pragmas);
- **empirically** (`tests/encryption.rs::fsqlite_pragma_key_does_not_encrypt`):
  a database opened with `PRAGMA fsqlite.key = '...'` (and `PRAGMA key`),
  with a marker row inserted, has the marker **in plaintext in `probe.db` and
  `probe.db-wal`**. fsqlite also creates `-shm`, `-wal-cert`,
  `-wal-cert-head`, `-fsqlite-ns-gate`, `-fsqlite-ns-use` and
  `.fsqlite-migration-state` sidecars next to the database.

So the pragma does **not** encrypt, and genome-cli uses its own encryption
layer with the same primitives (crates `chacha20poly1305`, `argon2`,
`zeroize`). fsqlite remains the SQL engine. The test fails if a future
fsqlite starts encrypting; that is the signal to revisit this design.

## 2. Design

| what | file(s) | protection |
|---|---|---|
| kit database (kit names, sample ids, source paths, summaries) | `<data_dir>/genome.db` | sealed container, in-memory fsqlite |
| genotype stores | `<data_dir>/kits/<id>/{sites.bin,heap.bin,rsid.idx,contigs.json}` | chunked AEAD, random access |
| audit log | `<data_dir>/audit.log` | per-record AEAD, hash-chained |
| exports (`--output`) | user-chosen | plaintext with a warning; `--encrypt-output` seals with a passphrase |
| pipeline outputs | `--out DIR` | external tools write plaintext; `pipeline run --seal` (below) |
| lock file | `<data_dir>/genome.db.lock` | empty (advisory lock only) |

All encryption is **XChaCha20-Poly1305** (256-bit key, random 192-bit nonce
per message/chunk, 128-bit tag). Decryption failures are reported as one
error, exit code **10** (`crypto`), without distinguishing a wrong key from
tampering: *"decryption failed (wrong key, or the file was modified,
truncated or corrupted)"*. A wrong passphrase is caught earlier, when the DEK
is unwrapped: *"wrong key: the database key could not be unwrapped"*.

### Kit database: sealed container

fsqlite cannot encrypt pages and always writes a WAL and sidecars, so the
database never lives in a file. On open, `genome.db` is decrypted into memory
and loaded into an **in-memory fsqlite database** (`:memory:`, which creates
no files; the tests check this). After every committing write (an autocommit
statement or a transaction commit) the whole database is dumped logically
(typed rows of every table, in `store::TABLES`) and re-sealed atomically:
write `genome.db.tmp<pid>`, fsync, rename, fsync the directory. A crash
leaves the old or the new version, never a mix and never plaintext.

```text
genome.db = "GNMDBSE1" | u32 len | envelope JSON | nonce | ciphertext | tag
            AEAD key = DEK, AAD = everything before the nonce (magic + envelope)
```

The envelope (key metadata, below) is authenticated but not secret. An
exclusive advisory lock on `genome.db.lock` serializes concurrent
`genome` processes, so two writers cannot lose each other's update.

The kit database holds metadata only (one row per kit; genotypes are in the
stores), so a full re-seal is cheap. See *Performance*.

### Genotype stores: chunked AEAD

Each store file is the same byte layout as before (see `src/gtstore.rs`),
wrapped in a sealed file:

```text
header (32 B): "GNMSEAL1" | chunk_size u32 (65536) | version u32 | file_id [16 random]
chunk i:       nonce [24] | ciphertext (64 KiB; last chunk 0..64 KiB) | tag [16]
AAD(i)       = header | label (file name, e.g. "sites.bin") | i (u64) | is_last (u8)
```

- **Random access stays fast.** Lookups are binary searches with positioned
  reads. A read decrypts only the 64 KiB chunks it touches, and the last 4
  decrypted chunks are cached. A 5M-record lookup touches about 12 chunks.
- **Authenticated chunk index.** Each chunk's index and the "last chunk" flag
  are in its AAD, so chunks cannot be reordered, duplicated or moved between
  files, and truncation (even at a chunk boundary) fails. The last chunk is
  verified when the file is opened.
- **Role binding.** The file name is in the AAD, so swapping `sites.bin` and
  `heap.bin` fails, as does replacing either with another file sealed by the
  same key.
- Writes go to `<file>.tmp<pid>` and are renamed into place after fsync. An
  unfinished writer deletes its temp file, and plaintext never touches disk.
  Plaintext chunk buffers are zeroized when freed.

A whole kit directory could still be swapped for another kit's directory
from the same database (both are valid). That gives an attacker no new data,
and the kit database records each kit's record count.

### Audit log

`audit.log` records every command that reads or modifies personal data:
`import`, `kits`, `rm`, `summary`, `lookup`, `compare`, `export`, `decrypt`
and `db init|encrypt|rekey|unlock`. Each entry has a timestamp, the command,
`$USER`, kit ids and counts (records, rows, queries, export destination
type). **Never genotype values, rsids, names or paths.**

```text
"GNMAUDT1" | file_id [16]
record: seq u64 | len u32 | seal(DEK, AAD, entry JSON) | len u32
AAD = "genome-cli audit v1|" | file_id | seq | tag of the previous record
```

It is append-only: a writer locks the file, reads the previous record via
the trailing length, and appends one record with fsync. Every record is
chained to its predecessor's tag. `genome audit log [--limit N]` decrypts
and verifies the whole chain, so editing, deleting or reordering any record
gives a `crypto` error. **Limitation:** someone with write access can
truncate the tail, or delete the whole log, undetected. Use OS-level controls
or off-host log shipping if that matters to you.

### Exports

`--output FILE` with a report or export carrying personal data (`kits`,
`summary`, `genotypes`, `compare`, `audit`, `export`, `decrypt`) prints
`warning: writing plaintext health data to FILE`. `--encrypt-output` instead
writes `GNMEXPT1 | kdf params | seal(Argon2id(passphrase), ...)`. The
passphrase comes from `GENOME_EXPORT_KEY` or a prompt, and the file is read
back with `genome decrypt FILE`. The format is genome-cli's own, not age;
pipe `genome export ... | age -p > f.age` if you need age interop. Output to
stdout is never warned about, since redirecting it is the user's explicit
choice.

### Pipeline (FASTQ -> VCF)

minimap2/bwa-mem2, samtools and bcftools are external programs. They write
their intermediates (subsampled FASTQ, BAMs, raw/filtered VCFs, indexes,
logs) under `--out` **in plaintext**, and genome-cli cannot intercept that.
Therefore:

- keep `--out` on an **encrypted volume** (FileVault/APFS encryption,
  LUKS, ...), and
- use `genome pipeline run --seal`. After a successful run it seals the final
  VCF with the database key (`<sample>.vcf.gz.sealed`; read it back with
  `genome decrypt`). Then it overwrites and deletes every other file under
  `--out`, keeping only the sealed file and `import.json`. The imported kit
  is already encrypted in the store. Sealing gives up resumability: a later
  run starts from scratch.

### Caches

`cache_dir` holds the UCSC chain files, the GRCh38 reference FASTA and its
aligner indexes, and the dbSNP rsid index. These are **public reference
data, not derived from any person's data**, so they are not encrypted (the
plaintext-scan test checks the cache for personal markers). genome-cli writes
no cache derived from personal data.

## 3. Keys

Envelope encryption: each database has a **random 256-bit DEK** (data
encryption key) that encrypts the database, stores and audit log. The DEK is
stored only **wrapped** (AEAD-encrypted) by a KEK (key encryption key) in the
envelope:

```json
{"v":1,"cipher":"xchacha20poly1305","db_id":"…","kek":"passphrase",
 "kdf":{"alg":"argon2id","m":65536,"t":3,"p":1,"salt":"…"},
 "wrapped_dek":"…","created_at":"…"}
```

KEK sources (the envelope records which one a database uses):

1. **OS keyring** (`kek: keyring`): a random 256-bit KEK in the macOS
   Keychain (Security framework, via the `keyring` crate's `apple-native`
   backend) or the Linux Secret Service (GNOME Keyring/KWallet over D-Bus),
   service `genome-cli`, account `db:<db_id>`. Nothing to type; the OS
   protects it with your login.
2. **Environment** `GENOME_KEY`: a passphrase, for CI and scripts.
3. **Interactive passphrase** (`kek: passphrase`), prompted without echo:
   KEK = Argon2id(passphrase, 16-byte random salt), m = 64 MiB, t = 3, p = 1.

Which source a **new** database uses is set by config `kek` (env
`GENOME_KEK`, flag `db init|encrypt|rekey --kek`):

- `auto` (default): `GENOME_KEY` if set, else the OS keyring, else a prompt;
- `keyring`;
- `passphrase`.

To **unlock** a passphrase database, genome-cli tries a session cached by
`genome db unlock`, then `GENOME_KEY`, then a prompt. With no terminal and no
key, it fails (exit 10) and names `GENOME_KEY`.

Commands:

| command | |
|---|---|
| `genome db init [--encrypt] [--kek …]` | create a database, **encrypted by default** (implicit creation on first `import` is encrypted too) |
| `genome db encrypt [--kek …]` | migrate a plaintext database in place (see below) |
| `genome db rekey [--kek …]` | re-wrap the DEK under a new KEK (new passphrase from `GENOME_NEW_KEY` or a prompt), e.g. passphrase -> keyring; deletes the old keyring entry |
| `genome db unlock` / `genome db lock` | cache / forget the passphrase-derived KEK in the OS keyring (convenience for interactive use); `lock` also shreds stale temp files |
| `genome db status`, `genome doctor` | encryption state, cipher, KEK type and KDF parameters, sealed vs plaintext store files, audit log state |
| `genome audit log` | verify and show the audit trail |
| `genome decrypt FILE` | read back `--encrypt-output` / `--seal` files |

`rekey` changes only the wrapping, which is what the envelope is for: it is
instant and does not rewrite gigabytes of stores. It does not help if the
**DEK** itself leaked (e.g. a memory dump). In that case re-import your files
into a new database, which gets a new DEK.

**Migration (`db encrypt`)** of a plaintext database:

1. Create the envelope and DEK. Write the sealed database to
   `genome.db.sealed-new` and **verify** it (reopen and compare the logical
   dump).
2. For every store file: seal it to `<file>.sealed-new`, **verify** (decrypt
   and compare SHA-256), overwrite the plaintext with random bytes, fsync,
   unlink, then rename the sealed file into place.
3. Move `audit.jsonl` entries into the encrypted log (then shred it).
4. Shred the plaintext `genome.db` and **all its sidecars** (`-wal`, `-shm`,
   `-journal` and fsqlite's `-wal-cert*`, `-fsqlite-ns-*`,
   `.fsqlite-migration-state`), then rename the sealed database into place.

If interrupted, rerunning `db encrypt` resumes with the staged database's
key. Running it on an encrypted database seals any plaintext stores left
behind.

**Plaintext mode** exists only behind an explicit `--insecure-plaintext`
flag (or `insecure_plaintext = true` / `GENOME_INSECURE_PLAINTEXT=1`). Every
run prints `warning: --insecure-plaintext: personal genomic data is stored
UNENCRYPTED on disk`. Without the flag, a plaintext database is refused with
a pointer to `genome db encrypt`.

`GENOME_INSECURE_FAST_KDF=1` lowers Argon2id to 1 MiB/1 pass for **newly
created** keys. It exists only so test suites run quickly. Never use it for
real data.

## 4. Threat model

**Protected (attacker gets the files at rest):** a stolen or lost laptop or
disk, backups (Time Machine, cloud sync of `~/.local/share`), another local
user reading your files, files copied off the machine. They get ciphertext;
the passphrase is protected by Argon2id, and a keyring KEK by the OS.
Integrity: any modification, truncation, reordering or swap of encrypted
data is detected (except audit-log tail truncation, above).

**Not protected:**

- **A compromised or malicious process running as you while the data is
  unlocked** (malware, a debugger). It can read memory, the keyring, or
  `GENOME_KEY`.
- **Memory.** Plaintext lives in process memory while a command runs. Keys
  and many plaintext buffers are zeroized on drop, but not every copy
  (parsed records, fsqlite's in-memory pages, output strings).
- **Swap and hibernation.** That memory can be paged to disk. Use encrypted
  swap (default on macOS; configure it on Linux).
- **External tools** in the FASTQ pipeline, and the **input files you
  import** (genome-cli does not delete your 23andMe/VCF downloads; store
  them on an encrypted volume or delete them yourself).
- **Plaintext you ask for:** stdout, `--output` without
  `--encrypt-output` (warned), `decrypt -o`, `--insecure-plaintext`.
- **Secure deletion.** "Overwrite then unlink" is best effort. SSD wear
  levelling, APFS/btrfs copy-on-write, snapshots and journals can keep old
  blocks. Full-disk encryption (FileVault, LUKS) is the real answer for
  remnants and is recommended in addition.
- **Metadata.** File sizes (≈ record counts), file times, the number of kits
  (directories) and the envelope (KDF parameters, KEK type) are visible.
- **`GENOME_KEY` in the environment** is visible to processes of the same
  user (e.g. `ps eww` on some systems) and may end up in shell history. Use
  it for CI, and the keyring or a prompt interactively.

**Access control** comes from the OS (file permissions, your login session
and keyring) plus the key: without the KEK, no genome-cli command can read
anything. The **audit trail** records who (`$USER`) ran which command on
which kits and when.

## 5. Performance

Measured on real data on an Apple Silicon Mac (release build, stable Rust,
encryption on, key in the macOS Keychain), 2026-10-04:

| operation | input | wall time | peak memory |
|---|---|---:|---:|
| `import` | 23andMe v5 array export, ~640k calls | 0.95 s | 61 MB |
| `import` | whole-genome VCF (.vcf.gz, ~430 MB), ~5.1M records | 24.2 s | 199 MB |
| `compare` | the two kits above, GRCh38 lifted to GRCh37, ~160k overlapping sites | 29.3 s | |

The resulting data directory (both kits, sealed database, audit log) is
301 MB, and a scan of every file for the sample id and rsids found no
plaintext. `cargo run --release --example encryption_bench -- 5000000`
reproduces the synthetic numbers.

The CLI's own commands (debug build, `tests/encryption.rs::performance_smoke`,
100k-record VCF) stay within a small factor of plaintext mode.

## 6. HIPAA

HIPAA's obligations (Security Rule, Privacy Rule, breach notification, BAAs)
apply to **covered entities and their business associates**, not to an
individual managing their own genome on their own laptop. genome-cli is not
"HIPAA compliant" by itself, and no software can be. Compliance is a
property of an organization's policies, risk analysis, training and
contracts. What this tool provides are the **technical safeguards** the
Security Rule describes (45 CFR 164.312): encryption at rest (addressable
specification 164.312(a)(2)(iv)), access control via keys bound to the OS
user/keyring (164.312(a)(1)), audit controls (164.312(b)) and integrity
controls through authenticated encryption (164.312(c)(1)). An organization
that adopts genome-cli still has to do its own risk analysis and cover the
items listed under *Not protected*.
