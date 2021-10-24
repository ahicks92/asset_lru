# asset_lru

Sometimes you want to cache assets from disk or somewhere else expensive.  Sometimes those assets are much smaller as
compressed/encoded bytes.  This crate provides a reasonably smart strategy for such cases, where the encoded bytes are
cached in memory as well as the decoded object.

This is very new, but with good code coverage via unit tests.
