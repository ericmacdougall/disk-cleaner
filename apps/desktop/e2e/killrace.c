/* The kernel check behind the write observer's "unreadable" calls (e2e/unreadable_control.py,
 * docs/DESKTOP.md, "A call strace could not read"): threads killed at a syscall entry stop.
 * Never shipped; CI builds it on the runner and runs it under each host's own strace.
 *
 * killrace <dir> <threads> <ms> <mode>
 *   Each thread loops mkdirat(dir, "<tid>-<i>") for i = 0, 1, ... and nothing else.
 *   mode "kill": after <ms> the main thread calls exit_group, which kills every thread
 *   wherever it is (some of them in a ptrace entry stop strace has not read yet).
 *   mode "sigkill": after <ms> the main thread sends its own process SIGKILL.
 *   mode "extkill": a child process sends this process SIGKILL after <ms>.
 *   mode "extkill2": the same, SIGKILL twice.
 *   mode "webkit": as WebKitGTK at close, a parent kills its child (which has a parent-death
 *   SIGKILL) and exits at once; the child's threads loop.
 *   mode "join": after <ms> the threads are told to stop and joined; nothing is killed.
 * A thread's call i is executed exactly when the folder "<tid>-<i>" exists, so a check can
 * tell whether the call strace could not read ran.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int dfd;
static volatile int stop;

static void *loop(void *arg) {
  (void)arg;
  long tid = syscall(SYS_gettid);
  char name[64];
  for (long i = 0; !stop; i++) {
    snprintf(name, sizeof name, "%ld-%ld", tid, i);
    mkdirat(dfd, name, 0700);
  }
  return NULL;
}

int main(int argc, char **argv) {
  if (argc != 5) { fprintf(stderr, "usage: killrace dir threads ms kill|join\n"); return 2; }
  dfd = open(argv[1], O_RDONLY | O_DIRECTORY);
  if (dfd < 0) { perror("open"); return 2; }
  int n = atoi(argv[2]);
  pthread_t t[256];
  if (n < 1 || n > 256) return 2;
  if (strcmp(argv[4], "webkit") == 0) {
    /* As WebKitGTK at close: the UI process kills its web process (which has a parent-death
     * SIGKILL) and exits at once. */
    pid_t c = fork();
    if (c == 0) {
      prctl(PR_SET_PDEATHSIG, SIGKILL);
      for (int i = 0; i < n; i++) pthread_create(&t[i], NULL, loop, NULL);
      for (;;) pause();
    }
    struct timespec w = {atoi(argv[3]) / 1000, (atoi(argv[3]) % 1000) * 1000000L};
    nanosleep(&w, NULL);
    kill(c, SIGKILL);
    _exit(0);
  }
  if (strcmp(argv[4], "extkill") == 0 || strcmp(argv[4], "extkill2") == 0) {
    /* Another process kills this one, as WebKit's UI process kills its web process;
     * "extkill2" sends SIGKILL twice, as a kill and a parent-death signal can. */
    pid_t me = getpid();
    int twice = strcmp(argv[4], "extkill2") == 0;
    if (fork() == 0) {
      struct timespec w = {atoi(argv[3]) / 1000, (atoi(argv[3]) % 1000) * 1000000L};
      nanosleep(&w, NULL);
      kill(me, SIGKILL);
      if (twice) kill(me, SIGKILL);
      _exit(0);
    }
    for (int i = 0; i < n; i++) pthread_create(&t[i], NULL, loop, NULL);
    for (;;) pause();
  }
  for (int i = 0; i < n; i++) pthread_create(&t[i], NULL, loop, NULL);
  struct timespec ts = {atoi(argv[3]) / 1000, (atoi(argv[3]) % 1000) * 1000000L};
  nanosleep(&ts, NULL);
  if (strcmp(argv[4], "kill") == 0) syscall(SYS_exit_group, 0);
  if (strcmp(argv[4], "sigkill") == 0) kill(getpid(), SIGKILL);
  stop = 1;
  for (int i = 0; i < n; i++) pthread_join(t[i], NULL);
  return 0;
}
