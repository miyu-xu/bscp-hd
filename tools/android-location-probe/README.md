# HD Android location probe

This test-only APK subscribes to `LocationManager.GPS_PROVIDER` and writes the latest real
framework callback to the app-private `files/location.txt`. Release gates read the file with
`run-as` and require exact latitude, longitude, altitude and accuracy values.

The probe never enables, creates or updates an Android mock provider. It is installed only in an
isolated test Guest and is removed after the gate. The checked-in APK fixture is architecture
independent and must be rebuilt from this source before its pinned SHA-256 changes.
