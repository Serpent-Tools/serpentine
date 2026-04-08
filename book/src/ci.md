# Using in CI

This is maybe the most important page in serpentine, because it is built to be runner agnostic, *especially in regards to caching*.

## \<Insert your CI runner here>

Running serpentine in Ci is as simple as ensuring a docker or podman daemon is available, and that you have serpentine installed in some manner, and then running:
```bash
serpentine run --ci
```

Now caching is where it gets fun, to persist serpentines layer caches between runners you just need to cache *one file*, however your CI platform does that.
Specifically if you use the following command:
```bash
serpentine run --cache /tmp/cache.serpentine --clean-old --standalone-cache --ci
```
You must restore `/tmp/cache.serpentine` before running it, and save it afterwards. 

## Github Actions

Using serpentine in github actions is no more special than any other runner, we use `actions/cache` to cache serpentines own cache file, and then simply run serpentine. 

```yaml
test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout
      - uses: actions/cache/restore
        id: cache-restore
        with:
          path: /tmp/cache.serpentine
          key: serpentine_cache-${{ github.sha }}
          restore-keys: serpentine_cache-

      - name: Install serpentine
        run: TODO_FOR_v1.0.0
      - name: Run serpentine pipeline
        run: serpentine run --cache /tmp/cache.serpentine --standalone-cache --clean-old --ci

      - uses: actions/cache/save
        if: always()
        with:
          path: /tmp/cache.serpentine
          key: ${{ steps.cache-restore.outputs.cache-primary-key }}
```

