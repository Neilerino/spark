// TAG [SPARK-154]
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sched.h>
#include <unistd.h>

#ifdef __APPLE__
extern int8_t generated_poll(void *) __asm__("_main");
#else
extern int8_t generated_poll(void *) __asm__("main");
#endif
extern void *__spark_alloc_state(int64_t);
extern void *__spark_spawn(void *, void *, int64_t, int8_t);
extern void *__spark_spawn_coop(void *, void *, int64_t, int64_t, int64_t);
extern void *__spark_await(void *);
extern void __spark_future_rc_dec(void *);
extern void __spark_rc_inc(void *);

static atomic_int release_child;
static atomic_int pending_count;
static atomic_int ready_count;
static atomic_int entered_count;

int64_t record_entered(void) {
    return atomic_fetch_add(&entered_count, 1) + 1;
}

static int64_t child_body(void *env) {
    (void)env;
    while (!atomic_load(&release_child)) sched_yield();
    return 7;
}

static int8_t recording_poll(void *state) {
    if (!atomic_load(&pending_count)) {
        if (generated_poll(state) != 0) abort();
        if (generated_poll(state) != 0) abort();
        atomic_store(&pending_count, 2);
        return 0;
    }
    int8_t status = generated_poll(state);
    if (status) atomic_fetch_add(&ready_count, 1);
    return status;
}

// Exit from the constructor so CRT never invokes the poll with a main ABI.
__attribute__((constructor)) static void drive_generated_poll(void) {
    alarm(10);
    void *child = __spark_spawn((void *)child_body, NULL, 8, 0);
    void *state = __spark_alloc_state(STATE_SIZE);
    memset(state, 0, STATE_SIZE);
    __spark_rc_inc(child); // Transfer a distinct owner into the generated frame.
    memcpy((char *)state + PARAMETER_OFFSET, &child, sizeof(child));
    void *parent = __spark_spawn_coop((void *)recording_poll, state, STATE_SIZE, 8, RESULT_OFFSET);
    while (atomic_load(&pending_count) != 2) sched_yield();
    atomic_store(&release_child, 1);
    int64_t result = *(int64_t *)__spark_await(parent);
    __spark_future_rc_dec(parent);
    // Keep an anchor until the generated parent is joined; cleanup is tested separately.
    __spark_future_rc_dec(child);
    printf("%lld %d %d %d\n", (long long)result, atomic_load(&pending_count),
           atomic_load(&ready_count), atomic_load(&entered_count));
    exit(result == 17 && atomic_load(&ready_count) == 1 && atomic_load(&entered_count) == 1 ? 0 : 1);
}
