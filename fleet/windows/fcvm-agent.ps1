# fcvm Windows guest agent. Started at every boot as SYSTEM by the fcvm-agent task.
# 1. Puts the static address on the virtio NIC (guest 10.0.2.100/24; fcvm's raw-VM namespace
#    has no gateway, the gateway and resolver are kept for the image build's QEMU network) and
#    pins the namespace bridge's neighbour entry to its fixed MAC (src/network/pasta.rs
#    NAMESPACE_MAC), as fc-agent does.
# 2. Ten times a second asks the VM's namespace (10.0.2.1:4600) for its restore generation and
#    the host's time ("<generation> <unix ms>"). A generation it has not seen means this VM was
#    restored from a snapshot (or just booted): it sets the wall clock to the host's time plus
#    half the round trip. Every answer is acknowledged on the same connection with
#    "ack <generation> <guest unix ms>", which fcvm waits for before it reports the VM healthy.
$ErrorActionPreference = 'SilentlyContinue'
function Net {
  foreach ($n in Get-NetAdapter | Where-Object { $_.InterfaceDescription -like '*VirtIO*' }) {
    $i = $n.ifIndex
    if (-not (Get-NetIPAddress -InterfaceIndex $i -IPAddress 10.0.2.100)) {
      Set-NetIPInterface -InterfaceIndex $i -Dhcp Disabled
      Get-NetIPAddress -InterfaceIndex $i -AddressFamily IPv4 | Remove-NetIPAddress -Confirm:$false
      Get-NetRoute -InterfaceIndex $i -AddressFamily IPv4 | Remove-NetRoute -Confirm:$false
      New-NetIPAddress -InterfaceIndex $i -IPAddress 10.0.2.100 -PrefixLength 24 -DefaultGateway 10.0.2.2 | Out-Null
      Set-DnsClientServerAddress -InterfaceIndex $i -ServerAddresses 10.0.2.2,10.0.2.3
    }
    Remove-NetNeighbor -InterfaceIndex $i -IPAddress 10.0.2.1 -Confirm:$false
    New-NetNeighbor -InterfaceIndex $i -IPAddress 10.0.2.1 -LinkLayerAddress 02-fc-00-00-02-01 -State Permanent | Out-Null
  }
}
Net
$gen = $null
$buf = New-Object byte[] 128
$ascii = [Text.Encoding]::ASCII
while ($true) {
  $c = New-Object Net.Sockets.TcpClient
  try {
    if ($c.ConnectAsync('10.0.2.1', 4600).Wait(200)) {
      $s = $c.GetStream(); $s.ReadTimeout = 200
      $sent = [DateTime]::UtcNow
      $n = $s.Read($buf, 0, $buf.Length)
      $f = $ascii.GetString($buf, 0, $n).Trim().Split(' ')
      if ($f.Length -ge 2) {
        if ($f[0] -ne $gen) {
          $rtt = ([DateTime]::UtcNow - $sent).TotalMilliseconds
          $now = [DateTimeOffset]::FromUnixTimeMilliseconds([int64]$f[1] + [int64]($rtt / 2)).UtcDateTime
          Set-Date -Date $now.ToLocalTime() | Out-Null
          if ($gen -ne $null) { Net }
          $gen = $f[0]
        }
        $ack = $ascii.GetBytes("ack $gen $([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())`n")
        $s.Write($ack, 0, $ack.Length)
      }
    }
  } catch {} finally { $c.Close() }
  Start-Sleep -Milliseconds 100
}
