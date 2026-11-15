// TAG [SPARK-117]
#include <stddef.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

void expect_string_rc(const void *const *slot, int64_t expected) {
    size_t actual = atomic_load((const _Atomic size_t *)*slot);
    if (actual != (size_t)expected) {
        fprintf(stderr, "String owners: %zu, expected %lld\n", actual, (long long)expected);
        // Stop before source Drops can consume an owner missing from the broken ABI.
        _Exit(73);
    }
}
