# Fixtures

Tiny archives made by external tools. Regenerate with the commands below.

`tree/` = `a.txt` ("hello"), `sub/b.txt` ("world"), `empty/`.

| File | Made with |
|---|---|
| `infozip.zip` | macOS Info-ZIP: `zip -qr infozip.zip tree` |
| `zipcrypto_infozip.zip` | `zip -qr -P secret zipcrypto_infozip.zip tree` |
| `slip.zip` | Python `zipfile`: entries `../evil.txt`, `/abs.txt`, `ok.txt` |
| `symlink_escape.zip` | Python `zipfile`: symlink `link -> ../../outside`, `link/pwn.txt`, symlink `good -> ok.txt`, `ok.txt` |
