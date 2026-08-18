# Tycho Storage

Tycho is an indexer designed to process and store data, necessitating the saving of state.

This tycho-storage crate handles all data storage and communication with the database.

## Architecture

### Database

Tycho currently uses PostgresSQL as its storage backend. The full schema can be found
in [schema.rs](./src/postgres/schema.rs).

Below is the Entity Relationship (ER) diagram illustrating the tables used for this project:

[![Entity Relation Diagram](../../docs/assets/tycho_db_er.png)](https://drive.google.com/file/d/1IQvdsfwRtg-AqtLuJjyGM2s6bqJGuciK/view?usp=sharing)

### Gateways

Database interactions are managed through multiple gateways,
including [cache](./src/postgres/cache.rs), [chain](./src/postgres/chain.rs), [contract](./src/postgres/contract.rs), [extraction_state](./src/postgres/extraction_state.rs)
and [protocol](./src/postgres/protocol.rs).

The CachedGateway serves as the main entry point for all database communications. It is designed to efficiently manage
and execute database operations by utilizing an in-memory cache and ensuring data consistency through transactional
writes. Writes are batched and deduplicated to improve performance and reduce load on the database.

### Versioning

Tycho employs a robust versioning system to track historical data within the database.
The [versioning](./src/postgres/versioning.rs) module provides tools to handle historical data, ensuring that each
version of an entity is tracked and stored appropriately.

#### Key Concepts

- VersionedRow: A trait for structs that can be inserted into a versioned table. It automates the valid_to attribute
  management, facilitating batch insertions.

- DeltaVersionedRow: Similar to VersionedRow, but also handles setting previous_value attributes, allowing for more
  complex versioning scenarios.

- StoredVersionedRow: A trait that enables setting the end version on currently active rows in the database based on new
  incoming entries. It's essential for ensuring that historical data is correctly marked as outdated when new versions
  are inserted.

# Development

## Postgres & Diesel

1. Install Postgres Library (macOS):

If you are on a mac, you might need to install postgres library first and add it to the library path:

```bash
brew install libpq
# PATH-TO-LIB looks somewhat like this: /opt/homebrew/Cellar/libpq/15.4/lib
export LIBRARY_PATH=<PATH-TO-LIB>:$LIBRARY_PATH
```

2. Install the Diesel CLI:

```bash
cargo install diesel_cli --no-default-features --features postgres
```

3. Start the Postgres Service:

```bash
docker-compose up -d db
```

4. Set Environment Variables:

```
export DATABASE_URL=postgres://postgres:mypassword@localhost:5431/tycho_indexer_0
export ETH_RPC_URL="url-here"

```

5. Setup/Update the Database:

```bash
diesel migration run --migration-dir ./tycho-storage/migrations
```

## Formatting

We use [pgFormatter](https://github.com/darold/pgFormatter) to keep SQL files consistently formatted.

### Setup pgFormatter with RustRover

1. Ensure you have pgFormatter installed:

```bash
brew install pgformatter
```

2. In RustRover, search for "External Tools" and add a new tool using the "+" button.
3. Get the path of pgFormatter installation:

```bash
which pg_format
```

4. Set the "Program" feild to this path.
5. Set the "Arguments" field to:

```bash
--no-space-function -i $FilePath$
```

6. Leave working directory empty.
7. Save the tool under "pgFormat" and add a shortcut if desired.

In case this works, but you get warning about your LOCALE settings, you can use
`/usr/bin/env` to set the locale and then invoke pgFormat as an argument to env:

    Program: `/usr/bin/env`

    Arguments: `LC_CTYPE=UTF-8 /opt/homebrew/bin/pg_format --no-space-function -i $FilePath$`

## Migrations

If you have to change the database schema, please make sure the down migration is included and test it by executing:

```bash
diesel migration redo --migration-dir ./tycho-storage/migrations
```

If the schema.rs file does not automatically update after you've run a migration with table changes, you can trigger the
update manually by executing:

```bash
diesel print-schema --config-file ./tycho-storage/diesel.toml > ./tycho-storage/src/postgres/schema.rs
```

## Tests

Currently Tycho exposes a single special [test-group](https://nexte.st/book/test-groups.html) via nextest:

1. `test(serial-db)`: These are tests against the database that need to commit data. To not intefere with other test
   that require a empty db but do not commit, we run these tests separately. Most of these tests use
   the `run_against_db` test harness. Test within that group are run sequentially, the remaining tests run in parallel.
   To add a test to this group simply ensure its name or its test package name includes the string `serial_db`.

If your test does not require committing to the database and has no special resource requirements, create the test as
usual.
