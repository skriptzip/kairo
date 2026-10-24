# Kairo server

Sync server for Kairo calendars. Stores data in SQLite, PostgreSQL or MariaDB.

## Run

```bash
docker compose up -d                                        # SQLite
DB_PASSWORD=secret docker compose -f docker-compose.postgres.yml up -d
DB_PASSWORD=secret docker compose -f docker-compose.mariadb.yml up -d
```

The image is `ghcr.io/skriptzip/kairo` (tags: `latest`, `sha-…`, `x.y.z`).
It listens on port 8080 and keeps SQLite data in the `/data` volume.

Without Docker: `cargo run --release`.

## Configuration

Environment variables:

| Variable | Default | Meaning |
| --- | --- | --- |
| `KAIRO_ADDR` | `127.0.0.1:8080` | Listen address (`0.0.0.0:8080` in the image) |
| `KAIRO_DATABASE_URL` | – | `sqlite://path`, `postgres://user:pass@host/db` or `mariadb://user:pass@host/db` |
| `KAIRO_DB` | `kairo.db` | SQLite file, used when no URL is set |
| `KAIRO_DB_POOL` | `10` | Connections for PostgreSQL / MariaDB |
| `KAIRO_REGISTRATION` | `open` | `open` or `closed` |
| `KAIRO_INVITE_CODE` | – | Required at registration if set |
| `KAIRO_TRUST_PROXY` | `0` | `1` behind a reverse proxy that sets `X-Forwarded-For` |

Tables are created on start. Switching databases starts empty; data is not migrated.

## Test

```bash
cargo test                                                   # SQLite in memory
TEST_DATABASE_URL=postgres://postgres:pw@localhost/kairo cargo test -- --test-threads=1
```

The test database is wiped first.

## Sicherheitshinweise

- **TLS:** Der Server spricht nur HTTP. Für Zugriff von außen immer einen TLS-Reverse-Proxy davorsetzen und `KAIRO_TRUST_PROXY=1` setzen. Die Compose-Dateien binden den Port bewusst nur an `127.0.0.1`. Die Clients lehnen `http://` für Hosts außerhalb von localhost/lokalem Netz ab.
- **Registrierung:** Nach dem Anlegen der Konten `KAIRO_REGISTRATION=closed` setzen oder einen `KAIRO_INVITE_CODE` verlangen.
- **Anfragegröße:** Request-Bodies sind auf 256 KiB begrenzt.
- **Kalender-Feed (`/ics/{id}?token=…`):** Das Feed-Token ist ein Zugangsschlüssel mit Lesezugriff auf *alle* Kalender des Kontos. Der Server speichert nur den SHA-256-Hash; das Token wird einmalig bei der Registrierung bzw. beim Erneuern (`POST /api/v1/auth/feed-token`) ausgegeben. Die URL wie ein Passwort behandeln und bei Verdacht erneuern. Falsche Tokens werden pro IP begrenzt (30 pro 15 Min.). Proxy-Logs sollten die Query-Strings von `/ics/` nicht aufzeichnen.
- **Sitzungs-Tokens** werden dagegen nur als SHA-256-Hash gespeichert.
- **Rate-Limit:** Die Login-/Registrierungs-Limits liegen im Arbeitsspeicher und werden bei einem Neustart zurückgesetzt. Hinter einem Proxy ohne `KAIRO_TRUST_PROXY=1` zählen alle Anfragen als eine IP; mit `KAIRO_TRUST_PROXY=1` ohne Proxy kann die IP über `X-Forwarded-For` gefälscht werden. Die Einstellung muss zur Umgebung passen.
- **E-Mail-Enumeration:** Registrierung (409) und Einladungen (404) verraten, ob eine Adresse registriert ist. Das ist bei einem kleinen privaten Server gewollt (Einladen setzt es voraus); bei öffentlicher Registrierung `KAIRO_REGISTRATION=closed` oder einen Einladungscode nutzen.
- **Image-Versionen:** Für Produktion statt `:latest` ein festes Tag (`ghcr.io/skriptzip/kairo:<version>`) verwenden.
- **Bekannte Advisories:** `cargo audit` meldet RUSTSEC-2023-0071 (`rsa`, über `sqlx-mysql`, nur relevant für MariaDB/MySQL-Logins per RSA-Austausch; kein Fix verfügbar) und eine Unsound-Warnung in `tokio` (`AsyncFd`, wird nicht verwendet). Wer SQLite oder PostgreSQL nutzt, ist nicht betroffen.
