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

Similar to buildkit/docker serpentine has dedicated support for github actions cache, but github does not expose the needed token to `run` steps, so we need to use `github-script` to expose them:

```yaml
test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout
      - uses: actions/github-script
        with:
          script: |
            core.exportVariable('ACTIONS_RESULTS_URL', process.env.ACTIONS_RESULTS_URL);
            core.exportVariable('ACTIONS_RUNTIME_TOKEN', process.env.ACTIONS_RUNTIME_TOKEN);

      - name: Install serpentine
        run: TODO_FOR_v1.0.0
      - name: Run serpentine pipeline
        run: serpentine run --cache-backend github --standalone-cache
```

Serpentine should be able to detect the backend automatically, but it doesnt hurt to set the backend explicitly. 
