# 0.1.1 (2021-11-07)

- We also need the right bound on the `Decoder` trait.

# 0.1.0 (2021-11-07)

- Add a seek bound to `VfsReader`.  This is for loading assets, and many decoders we might write require this.

# 0.0.3 (2020-10-24)

- Get rid of a stray `println`
- Errors now have a Display impl.
