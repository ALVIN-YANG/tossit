# TossIt

TossIt is an account-free messenger and transfer tool for devices that are
physically nearby. Its language separates durable identity and conversations
from temporary network observations and one-off transfers.

## Language

**Local device**:
This installation of TossIt together with its persistent cryptographic identity.
_Avoid_: Account, user

**Peer ID**:
The full public-key fingerprint that permanently identifies a local device until its identity is reset.
_Avoid_: IP address, Bluetooth identifier, device name

**Display ID**:
A short, human-readable prefix of a Peer ID used only for recognition and comparison.
_Avoid_: Peer ID, database key

**Trusted device**:
A local device whose Peer ID has been accepted for future conversations.
_Avoid_: Friend, account

**Nearby device**:
A temporary observation of a local device that is reachable through the current local network.
_Avoid_: Trusted device, contact

**Network space**:
A local conversation namespace created for one Wi-Fi only after TossIt successfully sends or receives text, an image, or a file there. Cellular, offline, permission-denied, and merely connected Wi-Fi states never create one.
_Avoid_: Saved Wi-Fi, system network history, contact list

**Transport endpoint**:
A current route to a local device, such as a LAN address or a Bluetooth connection.
_Avoid_: Identity, Peer ID

**Network conversation**:
A durable message history between local devices inside one network space. The same devices have separate histories on different Wi-Fi networks, while their Peer IDs and trust remain unchanged.
_Avoid_: Global conversation, Wi-Fi identity, friend list

**One-off transfer**:
A user-initiated transfer that ends after its selected payload succeeds, fails, or is cancelled and does not create a conversation.
_Avoid_: Bluetooth conversation, chat
