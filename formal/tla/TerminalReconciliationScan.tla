--------------------- MODULE TerminalReconciliationScan ---------------------
EXTENDS Naturals, FiniteSets

\* Scheduling only: Dispatch claims, physical occupancy and committed terminal
\* truth remain outside this cursor. No Dispatch transition is duplicated here.
\* Causes: finite stable ordered IDs; permanently failing/nonterminal prefix;
\* positive page budget; bounded restarts; fair periodic calls and reads that
\* return (including errors). Effects: every examined row spends budget and
\* advances, short/empty pages wrap, and healthy suffix rows are eventually seen.
\* This does NOT prove progress under infinite restarts, endless DB waits or
\* unbounded insertion before a candidate. Rust/SQL tests own adapter refinement.
CONSTANTS MaxRun, Budget, Failed, MaxRestarts, InitialAuthority
ASSUME /\ MaxRun \in Nat
       /\ Budget \in 1..256
       /\ Failed \subseteq 1..MaxRun
       /\ MaxRestarts \in Nat

VARIABLES cursor, busy, pageEnd, pageLength, visited, examined, healthySeen,
          restarts, wrapped, authority
vars == <<cursor, busy, pageEnd, pageLength, visited, examined, healthySeen,
          restarts, wrapped, authority>>
Min(a, b) == IF a < b THEN a ELSE b

Init == /\ cursor = 0 /\ busy = FALSE
        /\ pageEnd = 0 /\ pageLength = 0 /\ visited = 0
        /\ examined = {} /\ healthySeen = {} /\ restarts = 0
        /\ wrapped = FALSE /\ authority = InitialAuthority

\* R1 free local lock -> one page. A competing try_lock returns without work;
\* its stutter cannot authorize another scan or mutate any durable authority.
Begin == /\ ~busy
         /\ busy' = TRUE
         /\ pageEnd' = Min(cursor + Budget, MaxRun)
         /\ pageLength' = Min(Budget, MaxRun - cursor)
         /\ visited' = 0
         /\ UNCHANGED <<cursor, examined, healthySeen, restarts, wrapped, authority>>

\* R2 success/error/nonterminal all advance BEFORE the next asynchronous read.
\* Only successful healthy observations affect healthySeen, never authority.
Visit == /\ busy /\ visited < pageLength
         /\ LET row == pageEnd - pageLength + visited + 1 IN
              /\ cursor' = row
              /\ examined' = examined \cup {row}
              /\ healthySeen' = IF row \in Failed THEN healthySeen
                                ELSE healthySeen \cup {row}
         /\ visited' = visited + 1
         /\ UNCHANGED <<busy, pageEnd, pageLength, restarts, wrapped, authority>>

\* R3 exact-full page retains the cursor; short/empty page wraps. A full final
\* page therefore needs the next empty page, as in the production algorithm.
Finish == /\ busy /\ visited = pageLength
          /\ busy' = FALSE
          /\ cursor' = IF pageLength < Budget THEN 0 ELSE cursor
          /\ wrapped' = (wrapped \/ pageLength < Budget)
          /\ UNCHANGED <<pageEnd, pageLength, visited, examined, healthySeen,
                          restarts, authority>>

\* R4 process restart forgets only local scheduling position. Bounded restarts
\* express eventual process stability; no durable cursor or new queue is needed.
Restart == /\ restarts < MaxRestarts
           /\ cursor' = 0 /\ busy' = FALSE
           /\ pageEnd' = 0 /\ pageLength' = 0 /\ visited' = 0
           /\ restarts' = restarts + 1
           /\ UNCHANGED <<examined, healthySeen, wrapped, authority>>

Next == Begin \/ Visit \/ Finish \/ Restart
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Visit) /\ WF_vars(Finish)
TypeOK == /\ cursor \in 0..MaxRun /\ busy \in BOOLEAN
          /\ pageEnd \in 0..MaxRun /\ pageLength \in 0..Budget
          /\ visited \in 0..pageLength /\ examined \subseteq 1..MaxRun
          /\ healthySeen \subseteq (1..MaxRun) \ Failed
          /\ restarts \in 0..MaxRestarts /\ wrapped \in BOOLEAN
PerRoundBudget == visited <= Budget
ScanPreservesAuthority == authority = InitialAuthority
Safety == TypeOK /\ PerRoundBudget /\ ScanPreservesAuthority
HealthySuffixEventuallyObserved == <>((1..MaxRun) \ Failed \subseteq healthySeen)
NoHealthySuffixAfterWrap == ~(wrapped /\ MaxRun \in healthySeen)
=============================================================================
