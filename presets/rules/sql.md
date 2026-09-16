## SQL

### Report

- SQL built by string concatenation or interpolation of a value that is not a
  fixed literal, instead of a parameterised query or a bound placeholder.
- A migration with no way back — no `down`, no reverse migration, no
  documented recovery — for a change that is not purely additive (a dropped
  column, a renamed table, a data backfill that cannot be replayed).
- DDL that is not idempotent where the migration tool does not itself
  guarantee run-once semantics: `CREATE TABLE` without `IF NOT EXISTS`,
  `ALTER TABLE ADD COLUMN` with no existence check, on a system that would
  error on retry.
- A foreign key added with no supporting index on the referencing column,
  which makes every join and every cascade a table scan.
- A column changed from nullable to `NOT NULL`, or a type narrowed, with no
  migration step backfilling or validating existing rows first.
- A `DELETE` or `UPDATE` with no `WHERE` clause where nothing in the diff
  explains why every row is the intended target — not a migration that adds a
  column (or narrows one) and then backfills every existing row in the same
  change, which is the correct way to prepare for the constraint that follows.

### Do NOT report

- String-built SQL where every interpolated value is a literal already fixed
  in the same statement — a column name in a code-generated migration, a
  table name from a closed enum the code controls.
- A migration tool that already tracks applied migrations and refuses to
  reapply them; idempotence concerns there are the tool's job, not this
  file's.
- Missing indexes on columns that are not part of a join, a foreign key, or a
  filter the diff shows is on a hot path.
- Down-migrations for a project whose own convention (visible in sibling
  migrations) is forward-only with backups instead.
- Naming conventions, formatting, or whether a constraint is named
  explicitly versus left to the database's default.
- Query performance concerns with no evidence of table size — an unindexed
  scan on a table the schema shows is small and rarely grows.
- A whole-table `UPDATE` backfilling a column the same migration just added
  or narrowed.
