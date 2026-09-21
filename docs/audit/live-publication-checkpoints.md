# Final live-publication checkpoints

The theme executor verifies the resolved configuration snapshot before staging and again immediately before publication, just as active content does. This closes the configuration-change window across the longer theme copy operation and applies during clean-active-source serving too.

Read notifications are ignored to prevent feedback, but closing a writable source handle is a completion event and triggers reconciliation. Automatic publication-I/O retries are bounded to roughly once per second while ordinary source events remain responsive. The checkpoint is still not a concurrent-hostile-writer transaction or an atomic HTML publication protocol.

The clean-source real-server test changes workspace configuration and theme input, proves the new theme is withheld, then restores the original configuration and observes recovery without restart. Git event tests distinguish read accesses from write-close completion.
