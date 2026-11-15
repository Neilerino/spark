extern void __spark_rc_inc(void *);
extern void __spark_future_rc_dec(void *);
void replace_future(void **slot, void *replacement) {
    __spark_rc_inc(replacement);
    void *previous = *slot;
    *slot = replacement;
    __spark_future_rc_dec(previous);
}

extern void *__spark_list_get(void *, long long);
void replace_nested(void *slots, void *replacement) {
    void **slot = *(void ***)__spark_list_get(slots, 0);
    replace_future(slot, replacement);
}
