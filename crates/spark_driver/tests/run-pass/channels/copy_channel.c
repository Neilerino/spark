extern void __spark_rc_inc(void *channel);
void *copy_channel(void *channel) { __spark_rc_inc(channel); return channel; }
