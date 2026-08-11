#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/random.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;

static void *worker(void *arg) {
    unsigned char buf[8];
    long id = (long)arg;
    pthread_mutex_lock(&lock);
    getrandom(buf, sizeof buf, 0);
    counter += 1;
    pthread_mutex_unlock(&lock);
    printf("thread %ld %02x%02x%02x%02x\n", id, buf[0], buf[1], buf[2], buf[3]);
    return NULL;
}

int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++)
        pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < 4; i++)
        pthread_join(t[i], NULL);
    printf("counter=%ld\n", counter);
    return 0;
}
