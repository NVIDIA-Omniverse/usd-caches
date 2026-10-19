# Hub in Docker

## Preamble

Hub can be deployed in a docker container on a single-user workstation. To simplify use, the following requirements should be met:

- There should be a directory for the Hub cache on the host system, owned by the user that will be running the Hub container.
- The path to this directory should be consistent for all Hub clients. It is strongly recommended for this path to be `/var/cache/hub`.
- The Hub container should have its network configured in `host` mode.

If a bare metal application will be connecting to Hub in a container, the Hub cache path must be the same on the host system as it is within the Hub container. That is, the directory should be `/var/cache/hub` on the host system.

However, if all Hub clients are in containers as well, the Hub cache directory may exist within the user's home directory tree and merely mapped into the Hub and client containers as `/var/cache/hub`.

Note that containerized deployments of Hub are assumed to always be running in shared mode.


### Sample Docker Commands

Including a bare metal client:

```
docker run --rm -v "/var/cache/hub":"/var/cache/hub" -u $(id -u ${USER}):$(id -g ${USER}) --network=host hub_workstation_cache
```

Only containerized clients:

```
docker run --rm -v "$HOME/.cache/ov/hub":"/var/cache/hub" -u $(id -u ${USER}):$(id -g ${USER}) --network=host hub_workstation_cache
```

## Client Configuration

Clients using the Hub client (packaged with the omni-client library) will also need to configure their environment so the Hub client can find Hub in this configuration. This should be done as follows:

- Install the Hub executable somewhere on the filesystem. For the sake of discussion, this will be `/usr/local/bin/hub`.

- Set the `OMNICLIENT_HUB_EXE` environment variable to the full path of the installed Hub executable. This tells the Hub client where a Hub executable is. This is needed because the Hub executable is used to communicate configuration information back to the Hub client even when Hub itself will not be running locally.

- Set the `HUB__CACHE__PATH` environment variable to the full path of the Hub cache. This tells the locally-installed Hub executable where to coordinate with the containerized version of Hub that will be doing the caching.

- Set the `HUB__ARGS__DETECT_ONLY` environment variable to `true`. This tells the locally-installed Hub executable that it should only be looking for a running Hub instance and should not start one if no Hub instance is found.

### Sample Client Environment

Here is a sample client environment. Note that it is not necessary to have every Hub client install the Hub executable in the same location.

```
HUB__CACHE__PATH=/var/cache/hub
HUB__ARGS__DETECT_ONLY=true
OMNICLIENT_HUB_EXE=/usr/local/bin/hub
```
