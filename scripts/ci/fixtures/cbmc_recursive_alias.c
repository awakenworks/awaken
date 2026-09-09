/* Cause/effect qualification: Leaf.parent -> Internal* and Internal.data ->
 * Leaf form a recursive type graph. START_INTERNAL selects the opposite
 * discovery order; both must reach the actual false assertion (exit 10), not
 * an SMT type-dependency exception (exit 6). The symbolic pointer may be
 * non-null, so a green result is also a qualification failure.
 */
struct Internal;
struct Leaf { struct Internal *parent; };
struct Internal { struct Leaf data; };
#ifdef START_INTERNAL
extern struct Internal *nondet_input(void);
#else
extern struct Leaf *nondet_input(void);
#endif
int main(void)
{
  __CPROVER_assert(nondet_input() == 0, "force typed pointer discovery");
}
