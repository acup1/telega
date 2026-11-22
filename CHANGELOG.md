# Changelog

## Unreleased

- [x] Extract image display, video decoding, playback controls and terminal overlays into the standalone `tgp-viewer` app and Rust library. Build Linux, macOS and Nix releases for the viewer independently of Telegram.
- [x] Fix the release packaging check for the system awk shipped with macOS.
- [x] Explain Telegram application credential restrictions at login when the API rejects this build.
- [x] Package release builds with application credentials so first-time users can sign in with just their Telegram account. Add native Linux and macOS archives and a Nix launcher.

- [x] Allow saved authorized sessions to start without an API hash; require it only before a new login and document the first-login limitation.

- [x] Publish telega and terminalib over SSH; replace vendored sources with a pinned Git dependency and a verified Nix source hash.
- [x] Read local API credentials at runtime when Git flakes omit the ignored build-time file, preserving environment and embedded-value precedence.

- [x] License telega under GNU GPL v3 and align Cargo/Nix metadata.

- [x] Focus the composer and place its cursor on the first click, including border/padding; dismiss blocking message/search/reaction menus while preserving action button hitboxes.
- [x] Clear read chat badges immediately, acknowledge only displayed message IDs with coalesced retries, process inbox read updates and reject stale unread snapshots.

- [x] Show forwarded-message sources with clickable user/group/channel names, background peer resolution and correct album/code layout; keep hidden sender names unlinked.

- [x] Flush downloaded media before validating size and publishing the file, fixing false incomplete-download failures and repeated preview retries; cover 32 downloads and 40 simultaneous TGP images.

- [x] Review fixes: cancel obsolete preview jobs, bound scheduling to available workers, inspect at most 66 nearby messages and share the TGP upload budget fairly.
- [x] Reuse framebuffer text storage, clear overlapping wide glyphs, block terminal controls in cell text and accept Kitty IME events with omitted modifiers.
- [x] Preserve live edits/deletions/reactions during history loads; cancel stale search, deduplicate sent attachments and retain correct reply/draft state across view changes.
- [x] Publish media downloads/conversions atomically, clean cancelled temporary work and recover incomplete avatar/PNG caches.
- [x] Interrupt video bridge sockets on close, preserve compatible seeking, bound complete PNG frames and stop audio/restore synchronized output on error.

- [x] Replace animated TGP frames before deleting the previous image, and batch text and graphics in one synchronized draw.
- [x] Paint reserved Nerd Font cells with the folder rail background before drawing icons, preventing a one-cell theme mismatch.
- [x] Accept ordinary UTF-8, Kitty CSI-u without associated text, and IME text events in the composer.

- [x] Localize chat chrome, profile, settings, stories, gifts, reactions, search, attachment menus and reply previews with real Telegram Desktop keys where available; keep Russian fallbacks for client-specific text.
- [x] Expand terminal LaTeX layout with scripts, indexed roots, matrices, aligned rows, binomials and over/underlines.
- [x] Add an optional read-only live API smoke test; verify story/profile pagination, search and language packs on a copied session, plus Russian TUI labels, Cyrillic physical shortcuts and clean terminal restoration in a PTY.
- [x] Put Gifts, Posts and Attachments on the profile's top level; paginate post thumbnails and add story reactions and replies.
- [x] Add Telegram language packs in settings, move the folder settings gear to the bottom, stabilize folder icons and show contact-specific avatar photos.
- [x] Add group author avatars, profile username copy, mouse cursor placement, physical-key shortcuts, inline fenced code and two-dimensional text math; budget TGP uploads per frame.
- [x] Show a scrollable stories strip above All chats, add paged pinned story posts and a Telegram gifts viewer to user profiles.
- [x] Search users, groups and channels through Telegram from the chat sidebar; open @username mentions using exact text hitboxes.
- [x] Put settings sections in a vertical list; fill the entire opaque folder rail and keep its icon backgrounds consistent.
- [x] Retry failed image previews after bounded delays, time out stuck requests, discard incomplete thumbnails, prioritize visible media and reserve separate worker slots for stickers.
- [x] Show the actual filename on document attachments, with control characters removed.
- [x] Keep the last message's media and comment hitbox inside read-only chats; render custom emoji in sidebar chat previews with visible-only RAM loading.
- [x] Add Telegram story list above chats, a full list popup, photo/video viewing and read/view updates.
- [x] Round avatars only in the chat list with a cached transparent PNG; keep header, profile and gallery photos square and unmodified.
- [x] Add a right sticker sidebar, centered profile popup, inline custom emoji drafts, cursor-aware composer and message editing; add Ctrl+F search and read-only chat permissions.
- [x] Keep message photos beneath a higher TGP context menu; support Telegram code fences, sticker-pack opening and basic peer story viewing.

- [x] Send custom reactions only through native sendReaction, without a local Premium gate or emoji-reply fallback; report server rejection and retain removal support for legacy reply-carriers
- [x] Preserve recognized CSI-u Esc through terminalib's final event filter so it actually leaves the composer; retain the draft/reply on Esc and resume with i, while keeping unknown escape packets suppressed
- [x] Hide the complete text fallback beneath ready TGP custom emoji, including wide/ZWJ glyphs; preserve it while loading, on failure and without TGP
- [x] Fix hanging indents and explicit continuation lines in nested/numbered Markdown lists
- [x] Disable graphical LaTeX rendering and its Matplotlib runtime dependency; display readable Unicode math as text, preserving the original Copy source
- [x] Consume MTProto updates instead of dropping the receiver: live messages, edits, deletions and reactions, with history-gap safety and scroll anchoring
- [x] Add a centered rounded settings window on the far-left hamburger, keyboard/mouse tabs and persisted media preferences
- [x] Keep composer focus after sending; support Shift+Enter with enhanced keyboard decoding, multiline drafts and keyboard-mode restoration
- [x] Open message actions on right click and Reply on double left click; keep Delete above the composer and show a separate local/everyone deletion confirmation
- [x] Render animated reaction chips; keep legacy compatible replies readable and remove only own carriers in the matching chat
- [x] Open profile avatars as a full-screen, lazily paginated photo gallery
- [x] Add mouse/keyboard Emoji, Stickers and Custom picker tabs, installed/recent/featured packs and progressive animated previews loaded only for visible cells
- [x] Send custom emoji without Premium as readable emoji plus a compatible document-ID link; render both native and compatible entities with inline animation and correct UTF-16 positions
- [x] Preserve emoji positions after partial downloads; isolate graphics IDs and refresh static sticker images when picker content changes
- [x] Render opaque code blocks with preserved indentation, optional language/highlighting and exact-source Copy buttons
- [x] Render Markdown blocks and TeX formula fragments, with asynchronous TGP MathText images, safe text fallback and Nix-bundled dependencies
- [x] Remove the app title; let the inner hamburger collapse chats to avatars, preserving the far-left application menu, search and independent scrolling
- [x] Always fetch Telegram's current history tail on Latest, even when the locally loaded window reports no newer pages
- [x] Exit cleanly on Esc/q only from All chats without an open conversation; otherwise navigate back, preserving text-input priority and terminal restoration
- [x] Decode large TGS once using its actual timeline, show the first frame progressively, and keep WEBM alpha with bounded 256px frames
- [x] Bundle Python with Lottie in Nix; recognize document-form stickers and keep usable previews after partial decoder errors
- [x] Make Latest clickable and load the real history tail after navigating to an old message
- [x] Show original-author/text or attachment snippets above replies; batch-fetch missing originals for visible messages and jump to their history on click
- [x] Fix empty/invisible folder icons, map standard folder emoji to Nerd Fonts, and keep ASCII fallbacks when disabled
- [x] Close media on a standalone Esc without waiting for another key, preserve Esc before mouse input, and allow cancellation during gallery/file loading
- [x] Remove image/album/loading labels; center a Nerd Font glyph or `!` when TGP is unavailable
- [x] Open chat profiles from the header, with paginated shared media/files/audio/links and safe file saving
- [x] Preview clickable Telegram theme links, including hidden entities, before applying/installing
- [x] Add a pinned-message bar/list, pin/unpin actions, and history navigation around old pins in both directions
- [x] Keep the chat list independently scrollable while a conversation is open
- [x] Autoplay and loop visible GIF/MP4 animations with bounded streaming buffers
- [x] Play round videos inline with a circular mask, shared audio exclusivity, pause and seeking
- [x] Keep video viewers open at EOF/failure; exclude covers, retry compatible decoding, recognize video/audio attributes with generic MIME
- [x] Enlarge playing round videos within the chat viewport, restoring compact size at EOF
- [x] Paste clipboard images/files and terminal URI/path lists into an explicit compressed-photo/original-file confirmation
- [x] Compress photos in RAM without modifying originals; clean only owned temporary clipboard files
- [x] Buffer bracketed paste, split UTF-8 and following keys in terminalib without accidental Enter actions
- [x] Toggle/remove own reactions; fetch Telegram's full allowed reaction catalog, custom emoji packs and account limits
- [x] Render custom/premium reaction emoji by document ID with clickable counters
- [x] Update outgoing delivery/read checkmarks asynchronously without reloading the conversation

- [x] Paginated message history
- [x] Minimal rounded UI without solid backgrounds
- [x] Inline stickers, including the first frame of animated stickers
- [x] Reliable image loading and web-preview images with correct aspect ratio
- [x] Telegram albums grouped into one message
- [x] Chat avatars and periodically refreshed presence
- [x] Chat search
- [x] Archive and Telegram folders in the working menu
- [x] Artifact-free TGP lifecycle and a centered full-screen viewer
- [x] Sending chat attachments
- [x] Always use Telegram thumbnails before downloading full-size media
- [x] Force full-screen media onto the top TGP layer
- [x] Load adjacent gallery media beyond the currently loaded history
- [x] Channel comments, attachments, and linked chats without joining
- [x] Compatible changing TGP video frames
- [x] Redirect runtime errors away from the active terminal
- [x] Remove rendering artifacts from wide and composed emoji cells
- [x] Remove loaded-message counters from the status line
- [x] Vendor terminalib with Unicode framebuffer and explicit TGP z-index fixes
- [x] Keep chat previews in memory, persist only avatars, and store app data under `~/.cache/telega`
- [x] Remove accidental root-window borders that appeared as `│` and corner artifacts
- [x] Open image viewers from the in-memory preview before full media finishes loading
- [x] Stream video while downloading and add an interactive playback timeline
- [x] Remove stale per-process runtime media left by interrupted older launches
- [x] Download the thumbnail and full image concurrently in the media viewer
- [x] Center clickable video controls below the frame without image-wide pause clicks
- [x] Remove pseudographic message-box borders that left screen artifacts
- [x] Add one-at-a-time inline voice/audio players with per-message seeking
- [x] Keep the message composer inside the chat pane
- [x] Render Telegram custom emoji entities inline through TGP
- [x] Repaint changed terminal rows atomically to remove text trails while scrolling
- [x] Make the video timeline seekable and retry videos that need a complete file index
- [x] Add message reactions and a left-click action menu with reply, copy, forward, and delete
- [x] Restore rounded chat-list/chat panels while keeping individual messages borderless
- [x] Move folders into a Telegram Desktop-style vertical rail, preserving order and shared folders
- [x] Display static and animated stickers directly in the message list, without a photo viewer
- [x] Import official Desktop palettes, theme ZIPs, and t.me/addtheme links; persist theme selection
- [x] Add optional Nerd Font icons and fix wide-glyph continuations and cursor positioning
- [x] Put the dimming backdrop, media, and rasterized playback controls on separate TGP layers
- [x] Stream video through a seekable Telegram HTTP Range bridge with a bounded RAM cache
- [x] Add drag seeking and cancel blocked frame readers when closing or seeking
- [x] Add an attachment type menu and an in-app file browser with separate send confirmation
- [x] Embed local application API credentials at build time with runtime environment overrides
- [x] Track all current user requests in TASKS.md

## Next

- [ ] Apply Telegram updates instantly instead of polling metadata
- [ ] Verify live Telegram/TGP interactions in the user's terminal (automated decoder tests pass)
