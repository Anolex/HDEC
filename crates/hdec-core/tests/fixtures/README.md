# Fixtures

Tiny archives made by external tools. Regenerate with the commands below.

`tree/` = `a.txt` ("hello"), `sub/b.txt` ("world"), `empty/`.

| File | Made with |
|---|---|
| `infozip.zip` | macOS Info-ZIP: `zip -qr infozip.zip tree` |
| `zipcrypto_infozip.zip` | `zip -qr -P secret zipcrypto_infozip.zip tree` |
| `slip.zip` | Python `zipfile`: entries `../evil.txt`, `/abs.txt`, `ok.txt` |
| `symlink_escape.zip` | Python `zipfile`: symlink `link -> ../../outside`, `link/pwn.txt`, symlink `good -> ok.txt`, `ok.txt` |

TAR fixtures use `tree/` plus a 120-char directory `ddd…/ünï.txt` ("long") and symlink `link -> a.txt`.

| File | Made with |
|---|---|
| `bsdtar_pax.tar.gz` | `tar --format pax -czf … tree` (macOS bsdtar 3.5.3) |
| `gnutar.tar.zst` | `tar --format gnutar -cf - tree \| zstd -19` |
| `bsdtar.tar.xz` | `tar -cf - tree \| xz -9` |
| `bsdtar.tar.bz2` | `tar -cjf … tree` |
| `evil.tar` | Python `tarfile`: `../evil.txt`, `/abs.txt`, symlink `link -> ../../outside`, `link/pwn.txt`, hard link `hard -> ../../etc/passwd`, `ok.txt`, a FIFO |
| `note.txt.{gz,xz,bz2,lzma}` | `gzip -9c` / `xz -c` / `bzip2 -c` / `lzma -c` of "hello stream" |
| `multi_member.gz` | two gzip members: "part1 " + "part2" |
| `multi_frame.zst` | two zstd frames: "hello stream" + " more" |
