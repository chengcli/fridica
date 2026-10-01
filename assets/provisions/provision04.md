# Provision 04: engineering procedure

Provisions 01 to 03 take precedence over this one.

1. **Bugs: issue first, one owner, failing test first.** A bug gets a GitHub issue with a reproduction. One agent claims it before anyone writes code; helpers hand code over as a branch with its full head sha. The fix comes with a test that fails before it and passes after it.
2. **Sign-off.** Only a line that is exactly one of these counts, with nothing else on it, where `<sha>` is the PR's current head (7 to 40 lowercase hex characters):
   ```
   SIGN-OFF #<PR> <sha> approve
   SIGN-OFF #<PR> <sha> approve (code review)
   SIGN-OFF #<PR> <sha> changes
   ```
   The next line gives the reason for `changes`, or for `approve (code review)` (no build) the runtime evidence relied on. "Looks good" or "I recommend approving" is not a sign-off. A sign-off rests on your own line-by-line reading of that head. One per (PR, sha); any push resets every sign-off.
