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
stored only **wrapped**, once per **key slot**, in the envelope. Any one slot
decrypts the data; adding or removing a slot never re-encrypts anything.

```json
{"v":2,"cipher":"xchacha20poly1305","db_id":"…","created_at":"…","slots":[
  {"id":"ssh-1a2b3c4d","kind":"ssh","wrapped_dek":"…(age)…",
   "ssh_public_key":"ssh-ed25519 AAAA… you@laptop",
   "ssh_fingerprint":"SHA256:…","ssh_identity":"/home/you/.ssh/id_ed25519"},
  {"id":"file-5e6f7a8b","kind":"file","wrapped_dek":"…"}]}
```

Slot kinds:

1. **`ssh`** (the default): the DEK is encrypted to your **SSH public key**
   with [age](https://age-encryption.org) (`ssh-ed25519` and `ssh-rsa`
   recipients, the `age` crate). Your SSH private key decrypts it. Pure Rust,
   so the same OpenSSH key works on macOS, Linux and Windows
   (`%USERPROFILE%\.ssh`).
2. **`file`**: a random 256-bit KEK in a key file, like an ssh private key:
   `~/.config/genome-cli/keys/<db_id>.key` (Linux and macOS; under
   `$XDG_CONFIG_HOME` when set) or `%LOCALAPPDATA%\genome-cli\keys\` on
   Windows, never inside the data directory. The key directory is 0700 and
   the file 0600 (a protected, current-user-only ACL on Windows). Like ssh,
   genome-cli **refuses a key file other users can read** and says how to fix
   it. `GENOME_KEY_FILE` names a different file; `GENOME_KEY_DIR` moves the
   directory.
3. **`passphrase`**: KEK = Argon2id(passphrase, 16-byte random salt),
   m = 64 MiB, t = 3, p = 1. The passphrase comes from `GENOME_KEY` (CI,
   scripts) or a prompt without echo.
genome-cli **never uses an OS keychain or credential store** (macOS
Keychain, Secret Service, Windows Credential Manager): keys are files you
control and can carry to any machine, and nothing ever pops up a keychain
prompt. Databases made by 0.2 and earlier kept their key in the OS keychain;
they are recognised and refused with an explanation (to keep such data:
with genome-cli 0.2 run `genome db rekey --kek passphrase`, then open it here
with `GENOME_KEY` and run `genome db rekey --to ssh`).

**First run.** The first command that needs a database creates one. With no
`GENOME_KEY`, genome-cli looks for your SSH key (`GENOME_SSH_KEY`, else
`id_ed25519` then `id_rsa` in `~/.ssh`, or `GENOME_SSH_DIR`), says which key
and fingerprint it will use, and asks **"Encrypt with this SSH key? [Y/n]"**.
Without a terminal it goes ahead and prints the same notice to stderr. If the
answer is no, or there is no SSH key, it makes a key file slot instead.

If your SSH key has a **passphrase**, genome-cli adds a `file` slot as well,
so everyday commands never ask for the SSH passphrase. The SSH key stays the
**recovery key**: if the key file is lost, the SSH key (and its passphrase,
from a prompt or `GENOME_SSH_PASSPHRASE`) still opens the data. The notice
says so.

With `GENOME_KEY` set, a new database gets a passphrase slot instead (you
chose that key).

**The config file says how to get your data back.** genome-cli keeps an
`[encryption]` section at the end of its own config file
(`~/.config/genome-cli/config.toml` on Linux, `~/Library/Application
Support/genome-cli/config.toml` on macOS, `%APPDATA%\genome-cli\config.toml`
on Windows; `genome config path`). It lists the data directory, every slot,
the SSH fingerprints, public keys and private-key paths, the key files, and
step-by-step recovery instructions. It is rewritten whenever keys change,
contains no secrets, and is ignored as configuration.

**Unlock order:** key file (if present) -> SSH private key (unencrypted:
silent; passphrase-protected: `GENOME_SSH_PASSPHRASE` or a prompt) ->
`GENOME_KEY` -> passphrase prompt. If nothing works it fails (exit 10) and says, for each slot, what
was missing.

Which key a **new** database (or `rekey`) uses is set by config `kek` (env
`GENOME_KEK`, flag `db init|encrypt|rekey --kek`): `auto` (default, as
above), `ssh`, `file`, `passphrase`.

Commands:

| command | |
|---|---|
| `genome key status` | every slot, whether it is usable on this machine (SSH private key present, key file present and private) |
| `genome key add-ssh KEY.pub` | let another SSH key decrypt the data (a `.pub` file or the key line), e.g. a second machine or a backup key |
| `genome key add-passphrase` | add a passphrase slot (`GENOME_NEW_KEY` or a prompt) |
| `genome key add-file` | add a key file slot |
| `genome key remove SLOT` | remove a slot by id (or kind, if there is only one); never the last |
| `genome db init [--kek …]` | create a database, **encrypted by default** (implicit creation on first `import` is encrypted too) |
| `genome db encrypt [--kek …]` | migrate a plaintext database in place (see below) |
| `genome db rekey --to ssh\|file\|passphrase` | replace every slot (`--to` is `--kek`); deletes key files no longer used |
| `genome db lock` | shred stale temporary files |
| `genome db status`, `genome doctor` | encryption state, every slot, sealed vs plaintext store files, audit log; `doctor` also says which slot unlocks the data here |
| `genome audit log` | verify and show the audit trail |
| `genome decrypt FILE` | read back `--encrypt-output` / `--seal` files |

Version-1 envelopes (0.1/0.2, one KEK) are read as a single slot and written
back as version 2 the next time the database is saved.

The config file is written owner-only (0600; a current-user ACL on Windows).

**Where the data is (macOS).** Releases up to 0.1 kept data in
`~/.local/share/genome-cli`; the native macOS location is
`~/Library/Application Support/genome-cli`, which is also the config
directory. genome-cli uses the old location while it holds `genome.db` and
the native one holds none (the config file being there does not count), and
it never creates a new, empty database in the native location while an old
store exists: it stops and says where your data is.

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
disk, backups (Time Machine, cloud sync of the data directory), another local
user reading your files (the data directory is also owner-only: 0700 with
0600 files on Unix, a protected current-user-only ACL on Windows), files copied off the machine. They get ciphertext:
the data directory never contains a key. Copying or syncing it alone gives
nothing; an attacker also needs your SSH private key (itself
passphrase-protected, if you protect it), the key file, or the passphrase
(Argon2id makes guessing slow).

**Key files and unencrypted SSH keys are like ssh keys**: they protect the
data against everything above, but **not against someone who can already
read your files as you** (they can read the key too). That is the same
trade-off ssh makes, and it is what lets genome-cli work without prompts. If
you need protection against that, use a passphrase-protected SSH key and
remove the key file slot (`genome key remove file`; you will be asked for the
SSH passphrase each run), or a passphrase slot.
Integrity: any modification, truncation, reordering or swap of encrypted
data is detected (except audit-log tail truncation, above).

**Not protected:**

- **A compromised or malicious process running as you while the data is
  unlocked** (malware, a debugger). It can read memory, your SSH key and key
  files, or `GENOME_KEY`.
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
  blocks. Full-disk encryption (FileVault, LUKS, BitLocker) is the real answer for
  remnants and is recommended in addition.
- **Metadata.** File sizes (≈ record counts), file times, the number of kits
  (directories) and the envelope (KDF parameters, KEK type) are visible.
- **`GENOME_KEY` in the environment** is visible to processes of the same
  user (e.g. `ps eww` on some systems) and may end up in shell history. Use
  it for CI, and an SSH key, key file or prompt interactively.

**Access control** comes from the OS (file permissions, your login session)
plus the keys: without one of the key slots, no genome-cli command can read
anything. **Lose every key and the data is unrecoverable**: back up your SSH
key (or add a second one with `genome key add-ssh`). The **audit trail** records who (`$USER`) ran which command on
which kits and when (`$USER`, or `%USERNAME%` on Windows).

## 5. Performance

Measured on real data on an Apple Silicon Mac (release build, stable Rust,
encryption on), 2026-10-04:

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
user and key (164.312(a)(1)), audit controls (164.312(b)) and integrity
controls through authenticated encryption (164.312(c)(1)). An organization
that adopts genome-cli still has to do its own risk analysis and cover the
items listed under *Not protected*.
