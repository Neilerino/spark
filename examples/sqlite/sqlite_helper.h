// Minimal C helper that wraps sqlite3 for testing from Spark.
// Returns long (64-bit) to match Spark's Int type.

// Opens an in-memory database. Returns 0 on success, non-zero on failure.
long spark_sqlite_open_memory(void);

// Executes CREATE TABLE + INSERT + SELECT, returns the queried value (42).
// Returns -1 on error.
long spark_sqlite_roundtrip(void);

// Closes the database. Returns 0 on success.
long spark_sqlite_close(void);

// Returns the sqlite3 version number (e.g. 3039004).
long spark_sqlite_version_number(void);
