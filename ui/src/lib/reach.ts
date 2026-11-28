/** Whether a Space's origin reaches only the devices that reach this machine.
 *
 * A Local Space's browser host is a `*.localhost` name the Space generates; a
 * Space opened directly answers on a loopback address instead. Anything else
 * is reachable over the network, so its invitation link travels.
 */
export function reachesOnlyLocally(hostname: string): boolean {
  return (
    hostname === "127.0.0.1" ||
    hostname === "localhost" ||
    hostname === "::1" ||
    hostname === "[::1]" ||
    hostname.endsWith(".localhost")
  );
}
