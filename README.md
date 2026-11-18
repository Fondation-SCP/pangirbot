# Pangirbot

Automated actions for Wikidot wikis.

This is developed for the [French-speaking branch of the SCP Foundation](https://fondationscp.wikidot.com), and is named after [one of its memes](https://fondationscp.wikidot.com/le-pangirot).

## Usage

The following environment variables must be provided:

- `WIKIDOT_USERNAME`: the Wikidot username to be used;
- `WIKIDOT_PASSWORD_FILE`: the path to a secure file containing the Wikidot password to be used.

`.env` files are supported. `.env.example` contains an empty template that you can copy to `.env` and fill in with the credentials to your bot account.

### Security

The file pointed to by `WIKIDOT_PASSWORD_FILE` must be readable only by the user it is attached to, not by anyone else. This usually means Unix modes `0600` or `0400`.
