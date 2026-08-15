------------------------- MODULE LiveInboxProof -------------------------
EXTENDS LiveInbox, TLAPS

\* An invalid reorder is not a best-effort prefix application.  It is one
\* atomic rejected transition over the complete inbox state.
THEOREM RejectedReorderStutters ==
    \A candidate \in BoundedOrders:
      ~ExactPermutation(candidate, queue) /\ Reorder(candidate)
        => UNCHANGED vars
BY DEF Reorder

\* The transition has only two queue outcomes: the complete requested order,
\* or the exact old queue.  A partially applied prefix is unreachable.
THEOREM ReorderIsAllOrNothing ==
    \A candidate \in BoundedOrders:
      Reorder(candidate) => queue' = candidate \/ queue' = queue
BY Z3T(30) DEF Reorder, vars

=============================================================================
