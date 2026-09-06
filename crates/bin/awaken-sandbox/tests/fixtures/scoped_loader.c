/* B17 loader-time execution is workload code, before main and before any
 * shell startup file. If LD_PRELOAD leaks into the trusted helper, it fails
 * here because that helper has not entered the requested scope yet. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

__attribute__((constructor)) static void require_scope_before_main(void) {
    const char *expected = getenv("EXPECTED_SCOPE");
    char membership[4096];
    FILE *input = fopen("/proc/self/cgroup", "r");
    if (!expected || !input || !fgets(membership, sizeof membership, input)) {
        _exit(91);
    }
    fclose(input);
    membership[strcspn(membership, "\n")] = '\0';
    if (strcmp(expected, membership) != 0) {
        _exit(92);
    }
}
