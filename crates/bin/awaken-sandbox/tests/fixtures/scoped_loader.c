/* B17 loader-time execution is workload code, before main and before any
 * shell startup file. If LD_PRELOAD leaks into the trusted helper, it fails
 * here because a bootstrap process (PID 1/2) is not the authorized workload,
 * even if it already entered the namespace. The workload must also differ
 * from the owner's namespace, so membership cannot be a host-side pretense. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

__attribute__((constructor)) static void require_scope_before_main(void) {
    const char *expected = getenv("EXPECTED_PARENT_NS");
    char membership[4096];
    ssize_t size = readlink("/proc/self/ns/pid", membership, sizeof membership - 1);
    if (!expected || size < 0 || getpid() <= 2) {
        _exit(91);
    }
    membership[size] = '\0';
    if (strcmp(expected, membership) == 0) {
        _exit(92);
    }
}
