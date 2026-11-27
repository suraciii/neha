# Storage

This guide shows where a Space keeps the bytes of its Files, and how to keep them in an S3-compatible bucket instead of the Space home. The record stays in the Space's database in both cases. `neha serve` prints where the Space stores its Files when it starts. The [Space design](../specs/space/design.md#storage) owns the contract, and [Send and receive Files](files.md) covers the File flow itself.

## Local storage

A Space keeps File bytes in `blobs` in its home (`NEHA_HOME`) by default. No configuration is needed.

A Space keeps one copy of each content, however many Files carry it. The bytes are named by their content, so the name does not depend on the store.

To state the default in the file, set `backend = "local"`:

```toml
[storage]
backend = "local"
```

## Use an S3-compatible bucket

Add a `[storage]` table to `config.toml` in the Space home (`$NEHA_HOME/config.toml`). The Space reads the file at start, so restart the Space after a change.

| Key | Needed | Meaning |
| --- | --- | --- |
| `backend` | no | `local` or `s3`; defaults to `local`. |
| `bucket` | with `s3` | The bucket that holds the objects. |
| `region` | with `s3` | The region the service expects. |
| `endpoint` | with `s3` | The service address; must start with `http://` or `https://`. |
| `prefix` | no | The part of the bucket this Space uses. Defaults to the whole bucket. |
| `profile` | no | The AWS profile the credential chain reads from the shared files; environment credentials win. |
| `virtual_host` | no | `true` for virtual-hosted-style requests; defaults to `false` (path style). |
| `sse_kms_key_id` | no | The KMS key for server-side encryption; see [Encryption](#encryption). |

A `prefix` must not start with `/` or contain `..` or `\`.

An `endpoint` must start with `http://` or `https://`. The endpoint in the file is the only address the Space uses for the File store; environment variables do not change it.

A key the Space does not know, a missing required key, or a value it cannot use stops the start, and the error names the key. A `local` backend accepts no S3 key.

The start also opens the store. A bucket the credentials cannot use, or credentials the service rejects, stops the Space from starting, and the error names the File store. A store that is only unreachable lets the Space start; File reads and writes then answer `unavailable`, and `neha chat files` still reads the record.

The start checks neither write permission nor the KMS key. The first upload reports either.

### AWS

This example sets a prefix and a profile:

```toml
[storage]
backend = "s3"
bucket = "acme-neha"
region = "eu-central-1"
endpoint = "https://s3.eu-central-1.amazonaws.com"
prefix = "spaces/acme"
profile = "acme-sso"
```

The `profile` line is optional.

### A self-hosted S3-compatible service

This example points at a local container:

```toml
[storage]
backend = "s3"
bucket = "neha"
region = "us-east-1"
prefix = "spaces/acme"
endpoint = "http://127.0.0.1:8333"
```

Set `region` to the value the service expects. A service that ignores the region still needs one, and `us-east-1` is a common choice.

Set `virtual_host = true` when the service requires virtual-hosted-style addressing.

## Check the store

At start, `neha serve` prints where File bytes go. A local Space prints `Files are stored in a directory in this Space`, and an S3 Space prints the bucket, the prefix, and the endpoint.

## Change the store

A new Space needs no migration. It can use an empty prefix in a new bucket or an unused prefix in a shared bucket, and it writes its first object there.

An existing Space finds its Files only in the store it wrote them to. After `backend`, `endpoint`, `bucket`, or `prefix` changes, the Space no longer finds the bytes it recorded earlier; the record still lists those Files, but their downloads fail.

To move an existing Space to another store:

1. Stop the Space.
2. Copy every object to the new store under the same relative key, for example `ab/<sha256>`.
3. Change `config.toml`.
4. Start the Space and download several Files.
5. Delete the old store only after every File you need reads back.

For local storage the objects are the files under `<NEHA_HOME>/blobs`; for S3 they are the keys under the Space's prefix.

## Credentials

The Space reads credentials from the AWS default chain: environment variables first, then the shared AWS config and credentials files, then the role of the machine that runs the Space.

`profile` only chooses the profile the chain reads from the shared files. Credentials already in the process environment win over the profile.

The default chain reads `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` from the environment of the process that runs `neha serve`.

Do not put an access key or a secret key in `config.toml`. The Space reads no credentials from that file.

## Encryption

Set `sse_kms_key_id` to a KMS key ARN to have the bucket encrypt each object the Space writes with that key. The Space asks for SSE-KMS on every write, and the bucket decrypts on read.

The key policy must allow the credentials the Space uses. Without `sse_kms_key_id`, the bucket applies its own default encryption.

A self-hosted service must support SSE-KMS when `sse_kms_key_id` is set. A service that rejects the headers fails the Space's writes.

## One prefix per Space

A bucket can hold several Spaces, each under its own prefix. Two Spaces must not share a prefix.

An empty `prefix` gives the Space the whole bucket. Use it only when the bucket belongs to one Space.

## Backup

A complete backup of a Space has two parts: the Space home and the objects in the File store. The Space home holds the record, the encrypted Vault and Provider keys, and each Hosted Computer's Workspaces; with local storage it also holds `blobs`.

A Local Computer is not part of a Space backup. By default, its pairing in `~/.config/neha/computer.json` and its Bots' Workspaces in `~/.neha/workspaces` sit on the machine, so back them up there.

| Store | What to copy |
| --- | --- |
| Local | The whole Space home. |
| S3 | The Space home and the objects under the Space's prefix. |

With local storage the objects are inside the home, so one copy covers both parts.

Copy the Space home while the Space is stopped, so the record and the objects agree.

The Space never overwrites or deletes an object, so an object you copied once stays current. A later backup adds only new objects.

Never delete an object from the prefix. The Space reads each recorded File from it.

To restore, put the Space home and the objects back, then start the Space with the same `config.toml`.

The Vault's master key must be the one the old Space used. A Space reads `NEHA_MASTER_KEY` when it is set, and otherwise the `secrets/master.key` file in its home. When the old Space ran with `NEHA_MASTER_KEY`, set the same value before the restore starts; otherwise the key file comes back with the home.

An S3 restore also needs read access to the bucket, and permission to decrypt with the KMS key when `sse_kms_key_id` is set.

A record without its objects loses the bytes of every File. Objects without the record lose the File names. Restore both.

## Run the S3 tests locally

The `neha-space` tests include S3 tests that run against a local SeaweedFS container. Set `NEHA_TEST_S3=1` to switch them on; with the variable unset, they skip.

These variables point the tests at the container:

| Variable | Meaning |
| --- | --- |
| `NEHA_TEST_S3` | Set to `1` to switch the S3 tests on; unset, they skip. |
| `NEHA_TEST_S3_ENDPOINT` | The service address, for example `http://127.0.0.1:8333`. |
| `NEHA_TEST_S3_BUCKET` | The bucket that holds the test objects. |
| `NEHA_TEST_S3_REGION` | The region the service expects; defaults to `us-east-1`. |
| `AWS_ACCESS_KEY_ID` | An access key for the default credential chain. |
| `AWS_SECRET_ACCESS_KEY` | The secret key for that access key. |

The last two variables feed the default credential chain that signs each request. SeaweedFS does not check the signature, but the client still signs every request.

On a real bucket, these are the real access keys. The tests read the two variables, and a Space can also take its credentials from a `profile` or the machine's role.

Start the store, set the variables, and run the tests:

```bash
eng/s3.sh up
export NEHA_TEST_S3=1
export NEHA_TEST_S3_ENDPOINT=http://127.0.0.1:8333
export NEHA_TEST_S3_BUCKET=neha
export NEHA_TEST_S3_REGION=us-east-1
export AWS_ACCESS_KEY_ID=neha
export AWS_SECRET_ACCESS_KEY=neha-secret
just test-s3
```

`just test-s3` runs the `neha-space` tests with these variables set, so the S3 tests run too. `eng/s3.sh down` removes the store, and `eng/s3.sh logs` prints its output.
