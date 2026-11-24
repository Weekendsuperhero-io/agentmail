# How subscription cleanup works

HeroMail uses a mailing list’s `List-Id` to group existing messages. It uses
the selected message’s one-click unsubscribe URL to ask the mailing service
to stop sending to that recipient. These two operations have separate
results, so you can tell whether the provider accepted the request and how
many stored messages HeroMail removed.
[RFC 2919, §6](https://www.rfc-editor.org/rfc/rfc2919.html#section-6) ·
[RFC 8058, §3](https://www.rfc-editor.org/rfc/rfc8058.html#section-3)

## Find a list and its unsubscribe sample

`top_subscriptions` groups messages by the identifier inside the `List-Id`
angle brackets. Comparison is case-insensitive and ignores the optional
display phrase outside the brackets. RFC 2919 also tells readers to ignore
whitespace inserted inside the brackets in transit. For example,
`List-Id: Weekly News <News.Example.com>` and `List-Id: <news.example.com>`
both identify `news.example.com`. The From address and display name provide
context. [RFC 2919, §3](https://www.rfc-editor.org/rfc/rfc2919.html#section-3) ·
[RFC 2919, §6](https://www.rfc-editor.org/rfc/rfc2919.html#section-6)

A list appears in `top_subscriptions` when at least one message carries both
its valid `List-Id` and one-click unsubscribe syntax. HeroMail chooses the
newest qualifying message as the sample. The list’s count includes its older
messages with the same identifier, including those with missing, malformed,
or expired unsubscribe links. A missing or ambiguous `List-Id` leaves a
message outside exact list matching. Use `top_mailing_lists` to find a list
whose stored messages lack a usable one-click sample.

## Unsubscribe using the sample

HeroMail re-fetches the sample using its mailbox, UID, and UIDVALIDITY. The
message must have one `List-Unsubscribe` field containing exactly one HTTPS
URI. A `mailto:` alternative is permitted. It also needs one
`List-Unsubscribe-Post` field with the value `List-Unsubscribe=One-Click`.
Before the HTTPS POST, HeroMail verifies a DKIM signature covering both
fields and requires explicit user consent. The URL identifies the list and
recipient to the mailing service. If mail from the same list reaches several
aliases, the chosen message’s link acts on the enrollment it represents.
[RFC 2369, §3.2](https://www.rfc-editor.org/rfc/rfc2369.html#section-3.2) ·
[RFC 8058, §§3–4](https://www.rfc-editor.org/rfc/rfc8058.html#section-3)

Discovery checks the header syntax. The unsubscribe call reports the actual
result, including whether the HTTPS endpoint accepted the request.

## Remove the accumulated messages

| User intent | HeroMail action |
| --- | --- |
| Stop future mail | Call `unsubscribe_message` with the selected `sample` and `confirmOneClick: true`. Leave `cleanup` omitted. |
| Stop future mail and permanently remove the backlog | Use the same sample and set `cleanup.deletion` to `permanent` after the user approves permanent deletion. |
| Permanently remove an identified list’s backlog independently | Call `delete_list_id` with the exact `listId` and `permanent: true` after the user approves permanent deletion. |

For `unsubscribe_message`, pass the account from discovery and copy
`sample.mailbox`, `sample.uid`, and `sample.uidValidity` into `mailbox`,
`uid`, and `expectedUidValidity`. This request uses a sample from `INBOX`
and requests permanent cleanup.

```json
{
  "account": "work",
  "mailbox": "INBOX",
  "uid": 42,
  "expectedUidValidity": 3857529045,
  "confirmOneClick": true,
  "cleanup": { "deletion": "permanent" }
}
```

Combined unsubscribe and cleanup requires that the same passing DKIM
signature also cover the sample’s single `List-Id`. HeroMail uses that ID
for the account-wide sweep through eligible storage folders, including
Archive. It confirms each candidate’s List-Id from live headers. Older
messages participate through the exact List-Id match, independent of their
unsubscribe headers. Cleanup normally follows a successful unsubscribe.
Set `cleanup.when` to `always` only when the user has explicitly approved
deletion regardless of the unsubscribe result.
Use `delete_list_id` for an explicitly identified list if the one-click
request failed or the mailing service stopped offering one-click.
[RFC 2919, §6](https://www.rfc-editor.org/rfc/rfc2919.html#section-6) ·
[RFC 6376, §5.4](https://www.rfc-editor.org/rfc/rfc6376.html#section-5.4)

## Free storage

Moving mail to Archive keeps the messages in the account. Moving mail to
Trash stages it for deletion. On a standard IMAP server with UIDPLUS,
an explicit permanent delete uses `UID EXPUNGE` on the selected UIDs.
When the sweep is account-wide, it also checks Trash for matching list mail.
Unrelated messages flagged `\Deleted` stay outside the expunge.
[RFC 4315, §2.1](https://www.rfc-editor.org/rfc/rfc4315.html#section-2.1)

HeroMail uses Gmail’s All Mail when it is visible over IMAP to find inbox
and archived messages. It moves matches to Trash and reports
`permanent: false` even when permanent deletion was requested. To reclaim
space immediately, use **Delete forever** for the
matching messages in Gmail Trash. Gmail also removes trashed messages after
30 days. Storage usage can take time to update after the messages are purged.
[Delete messages in Gmail](https://support.google.com/mail/answer/7401) ·
[Manage Google storage](https://support.google.com/mail/answer/6374270)
