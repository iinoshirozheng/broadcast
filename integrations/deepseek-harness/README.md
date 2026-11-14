# DeepSeek Harness plugin

This directory is an installable DeepSeek Harness bundle. It adds a Broadcast entry to the Harness sidebar and opens the active installed Broadcast runtime.

From a DeepSeek Harness source checkout, install it into the Web profile:

```sh
pnpm dsh plugin --profile web add /absolute/path/to/broadcast/integrations/deepseek-harness
```

Start Broadcast before opening the entry. The plugin reads the launcher-owned runtime file, so it does not depend on a fixed port.
