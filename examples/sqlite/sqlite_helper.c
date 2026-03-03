#include <sqlite3.h>
#include <stdlib.h>

static sqlite3 *g_db = NULL;

long spark_sqlite_open_memory(void) {
    return (long)sqlite3_open(":memory:", &g_db);
}

static long query_result = -1;

static int callback(void *unused, int argc, char **argv, char **col_names) {
    (void)unused;
    (void)col_names;
    if (argc > 0 && argv[0]) {
        query_result = atol(argv[0]);
    }
    return 0;
}

long spark_sqlite_roundtrip(void) {
    if (!g_db) return -1;
    char *err = NULL;

    int rc = sqlite3_exec(g_db, "CREATE TABLE test (id INTEGER, value INTEGER);",
                          NULL, NULL, &err);
    if (rc != SQLITE_OK) { sqlite3_free(err); return -1; }

    rc = sqlite3_exec(g_db, "INSERT INTO test VALUES (1, 42);",
                      NULL, NULL, &err);
    if (rc != SQLITE_OK) { sqlite3_free(err); return -1; }

    query_result = -1;
    rc = sqlite3_exec(g_db, "SELECT value FROM test WHERE id = 1;",
                      callback, NULL, &err);
    if (rc != SQLITE_OK) { sqlite3_free(err); return -1; }

    return query_result;
}

long spark_sqlite_close(void) {
    if (!g_db) return -1;
    long rc = (long)sqlite3_close(g_db);
    g_db = NULL;
    return rc;
}

long spark_sqlite_version_number(void) {
    return (long)sqlite3_libversion_number();
}
