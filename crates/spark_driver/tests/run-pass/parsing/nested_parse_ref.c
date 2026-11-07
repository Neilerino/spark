#include <stdint.h>

extern void *__spark_string_from_literal(const char *, int64_t);
static int64_t start = 1;
static int64_t *start_ref = &start;
static void *text_slots[4];
static void **text_ref = text_slots;

void *nested_start(void) { return &start_ref; }
void *nested_text(void) {
    text_slots[0] = __spark_string_from_literal("7", 1);
    return &text_ref;
}
