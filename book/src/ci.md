# Using in CI

This is maybe the most important page in serpentine, because it is built to be runner agnostic, *especially in regards to caching*.

## \<Insert your CI runner here>

Running serpentine in CI is as simple as ensuring a docker or podman daemon is available, and that you have serpentine installed in some manner, and then running:
```bash
serpentine run
```

Now caching is where it gets fun, to persist serpentines layer caches between runners you just need to cache *one folder*, however your CI platform does that.
Specifically if you use the following command:
```bash
serpentine run --cache-folder /tmp/serpentine_cache --standalone-cache
```
You must restore `/tmp/serpentine_cache` before running it, and save it afterwards. 

## Github Actions

Serpentine also has a dedicated github action: <https://github.com/marketplace/actions/run-serpentine>

