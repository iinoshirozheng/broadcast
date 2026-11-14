export function withoutBroadcastLauncherEnvironment(environment = process.env) {
  return Object.fromEntries(
    Object.entries(environment).filter(([name]) => !name.startsWith("BROADCAST_")),
  );
}
