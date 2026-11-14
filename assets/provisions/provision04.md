# Provision 04: engineering procedure

Provisions 01 to 03 take precedence over this one.

1. **Bugs: issue first, one owner, failing test first.** A bug gets a GitHub issue with a reproduction. One agent claims it before anyone writes code; helpers hand code over as a branch with its full head sha. The fix comes with a test that fails before it and passes after it (RED/GREEN).
2. **Sign-off.** Only a line that is exactly one of these counts, with nothing else on it, where `<sha>` is the PR's current head (7 to 40 lowercase hex characters):
   ```
   SIGN-OFF #<PR> <sha> approve
   SIGN-OFF #<PR> <sha> approve (code review)
   SIGN-OFF #<PR> <sha> changes
   ```
   When you are asked to review a PR, the first line of your reply is that sign-off line. The next line gives the reason for `changes`, or for `approve (code review)` (no build) the runtime evidence relied on. "Looks good" or "I recommend approving" is not a sign-off. A sign-off rests on your own line-by-line reading of that head. One per (PR, sha); any push resets every sign-off.

   Before you post one:
   - Write `VERDICT <owner>/<repo>#<PR> <sha> <verdict>` (the full 40-character sha you read, a verdict above) on a line of its own instead. Fridica reads the PR's head when it posts the message and puts the sign-off line first only if the head is still that sha; otherwise it says the head moved and does not sign. No re-read before posting is needed.
   - Sign off PRs of one repository per message, since `#<PR>` names no repository. PRs in another repository go in a separate message.
   - No other line of the message may start with "sign-off", in any case. Mention a sign-off mid-sentence instead.
   - The verdict is yours. A delegated worker never writes a sign-off line; one that read the head line by line may end its report with the verdict line, which Fridica signs as above.
