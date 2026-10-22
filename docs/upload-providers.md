# Upload providers

Research reviewed on **22 October 2026**. Limits below describe the services' published policies, not a promise of availability or permanent storage. No user files were uploaded during this review. An anonymous upload means no account is attached to the upload; the host can still receive the uploader's IP address.

## Choosing a destination

**Local only** and **Custom server** come first in setup. The larger public-host choices are services that need no credentials. Services requiring an account or API credential appear in a smaller **Account or API key** group. That label also covers Imgur: its Client-ID uploads are anonymous, but obtaining a client ID requires application registration.

| Destination | Setup               | File limit                                                          | Retention                                            |
| ----------- | ------------------- | ------------------------------------------------------------------- | ---------------------------------------------------- |
| Uguu        | No account or key   | 128 MiB                                                             | 3 hours                                              |
| Catbox      | No account or key   | 200 MB; GIFs 20 MB                                                  | Anonymous files removed after 2 years without access |
| Litterbox   | No account or key   | 1 GB, documented by its API client; see verification note below     | Choose 1, 12, 24 or 72 hours                         |
| Imgur       | Client ID           | Provider help: 50 MB still / 200 MB animated; API limits may differ | Host-managed; no lifetime requested                  |
| Pixeldrain  | Account and API key | Free plan: 10 GB; paid plans: 100 GB                                | Free plan: 60 days without qualifying access         |
| s-ul        | Account and API key | No current public numeric limit verified                            | Host-managed; no lifetime requested                  |

The **Litterbox retention** selector in setup and Settings offers 1, 12, 24 or 72 hours. Existing preferences default to 72 hours. This choice is saved independently of the custom-server lifetime, so switching destinations preserves exact custom values, including zero.

The **Delete upload after** setting applies to a custom server. **No expiry** leaves the lifetime unspecified; the server can still enforce its own retention. It cannot override Uguu's expiry, Catbox's inactivity policy, or another public host's rules.

## No-account services

**Uguu** publishes a 128 MiB maximum and three-hour retention. Its documented upload API accepts multipart `files[]` at `https://uguu.se/upload`, with optional `output=text`. It accepts general files subject to its content rules. Its FAQ says the upload record includes the IP address until expiry. [Homepage](https://uguu.se/), [API](https://uguu.se/api), [FAQ](https://uguu.se/faq).

**Catbox** publishes a 200 MB limit and a separate 20 MB GIF limit. Its FAQ excludes `.exe`, `.scr`, `.cpl`, `.doc*`, and `.jar`; it also restricts commercial hosting. Anonymous uploads omit `userhash` and submit `reqtype=fileupload` and `fileToUpload` to `https://catbox.moe/user/api.php`. The response is a plain URL. File deletion through the API requires an account hash, so anonymous uploads do not provide a self-service deletion capability in ShareR. [Homepage](https://catbox.moe/), [FAQ](https://catbox.moe/faq.php), [API](https://catbox.moe/tools.php).

Catbox's older FAQ and third-party integrations sometimes still say “forever.” The operator changed this policy effective **6 July 2026**: anonymous files with no access for more than two years are removed. ShareR therefore must not describe anonymous Catbox uploads as unlimited or permanent. [Operator's policy announcement](https://blog.catbox.moe/post/821058172332769280/removing-idle-uploads).

**Litterbox** is Catbox's temporary service. The documented client sends multipart `reqtype=fileupload`, `time=1h`, `12h`, `24h`, or `72h`, and `fileToUpload` to `https://litterbox.catbox.moe/resources/internals/api.php`, then reads a plain URL. ShareR exposes these four durations in both setup and Settings; there is no unlimited option. The client documents a 1 GB limit. [Client source](https://savolae.github.io/catbox/src/catbox/litter.rs.html), [provider tools page](https://litterbox.catbox.moe/tools.php).

**Verification limits:** Litterbox's pages returned HTTP 403 through the research browser or timed out directly. Its protocol and size limit were checked against the published client implementation, not a successful live upload. Catbox's official FAQ and API documentation were readable through the research browser, although direct requests still timed out. These results do not establish global availability or a shutdown.

Litterbox returns a URL without an expiry timestamp. History therefore records the selected duration as **requested**, rather than calculating an exact deletion time from the local clock. Uguu's three-hour limit is labeled **host policy**. Custom-server timestamps are displayed as returned by the server; an upload receipt is not proof that the host will delete every copy at that time.

## Services requiring setup

**Imgur** uses `Authorization: Client-ID …` and a multipart `image` upload to the documented `https://api.imgur.com/3/image` endpoint. This does not associate the image with an Imgur account. Current API documentation lists JPEG, JPG, GIF, PNG, APNG, and TIFF among image formats. ShareR accepts those documented formats and rejects unsupported formats such as WebP, SVG and AVIF before starting a request. This includes WebP recordings: choose another destination for them. Its general help lists 50 MB for still images and 200 MB for animated images. Anonymous still images above 1 MB may be compressed, and PNG files above 5 MB may become JPEG, so Imgur is a poor choice when preserving the exact original matters. Current API documentation does not repeat a numeric upload limit; the deprecated documentation's 10 MB figure should not be treated as a freshly verified limit. [Current API documentation](https://apidocs.imgur.com/), [upload limits and formats](https://help.imgur.com/hc/en-us/articles/26511665959579-What-files-can-I-upload-Is-there-a-size-limit).

**Imgur access restrictions:** the service has blocked access in the United Kingdom since 30 September 2025. An anonymous upload is not guaranteed permanent; inactive content may be removed. [UK access notice](https://help.imgur.com/hc/en-us/articles/41592665292443-Imgur-access-in-the-United-Kingdom), [terms update](https://help.imgur.com/hc/en-us/articles/26479362527771-Imgur-Terms-of-Service-Update).

**Pixeldrain** now explicitly requires an API key for uploads. Use HTTP Basic authentication with the key as the password, and multipart `file` at `https://pixeldrain.com/api/file`; the JSON response supplies the file ID. Share the viewer URL `https://pixeldrain.com/u/{id}`, because free accounts have hotlink restrictions. The free plan publishes a 10 GB file maximum and 60-day inactivity expiry. A qualifying download resets that timer; paid plans differ. The service's `limits` page was read directly over HTTPS. [API](https://pixeldrain.com/api), [plan limits](https://pixeldrain.com/limits), [retention and hotlinking](https://docs.pixeldrain.com/questions_and_answers/).

**s-ul** remains reachable and asks users to register or log in. It is retained for existing users in the smaller credential-based selection. Current public pages did not establish an upload-size or retention guarantee, so the UI must not invent one. [Service homepage](https://s-ul.eu/), [terms](https://s-ul.eu/terms).

## Retired choices and compatibility

**transfer.sh's public instance** is unsuitable as a default destination. The maintainer stated that uploads were disabled during an abuse investigation, and the hosted endpoint remained unreachable during this review. This is about the public instance, not the continued existence of the open-source server. [Maintainer's explanation](https://github.com/dutchcoders/transfer.sh/issues/594).

**vgy.me** could not be verified as a dependable application upload destination. Its homepage and upload endpoint returned Cloudflare HTTP 403, which is not proof of shutdown. ShareX still includes an adapter with an optional `userkey`, but that implementation alone does not demonstrate that anonymous uploads currently work. Remove it from new-user choices without claiming the company or website is dead. [Current ShareX adapter](https://github.com/ShareX/ShareX/blob/master/ShareX.UploadersLib/ImageUploaders/VgymeUploader.cs).

Retired provider identifiers and saved credentials should remain readable for compatibility. Attempts to use a retired destination must explain that another destination needs to be selected. Never silently send a user's files to a replacement host.

## Scope of the shortlist

ShareX's current destination list was used to discover established integrations, not as an uptime check. It still contains both vgy.me and transfer.sh. [ShareX destination enum](https://github.com/ShareX/ShareX/blob/master/ShareX.UploadersLib/Enums.cs).

**0x0.st is deferred**, despite its useful no-account API. It permits files up to 512 MiB and uses size-dependent retention, with optional shorter expiry. Owner deletion requires a token from the upload response and a subsequent HTTP POST to the file URL; some deduplicated uploads have no deletion token. ShareR currently models remote deletion as opening a deletion URL in the browser. Adding 0x0.st before implementing token-based deletion, token secrecy, and its per-receipt availability would misrepresent its capabilities. It also requires an identifying User-Agent and explicit user choice; it must not become a hidden fallback. [Provider API and terms](https://0x0.st/).

ImgBB is another straightforward credential-based image service: its API requires a key, permits images up to 32 MB, and supports optional expiry. Its current homepage advertises API access in Pro plans, so free API availability is not established here. It is a possible future addition rather than a no-setup anonymous choice. Dropbox, Google Drive, OneDrive, and similar ShareX destinations require broader account authorization and sharing flows; listing them as selectable providers before implementing those flows would be misleading. [ImgBB API](https://api.imgbb.com/), [ImgBB plans](https://imgbb.com/), [ShareX integrations](https://github.com/ShareX/ShareX/blob/master/ShareX.UploadersLib/Enums.cs).
