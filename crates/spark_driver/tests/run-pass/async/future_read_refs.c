// TAG [SPARK-114]
#include <stdint.h>
extern void *__spark_list_get(void *list, int64_t index);
extern int64_t __spark_string_len(void *string);
int64_t foreign_length(void *list) {
    void **slot = *(void ***)__spark_list_get(list, 0);
    return __spark_string_len(*slot);
}
