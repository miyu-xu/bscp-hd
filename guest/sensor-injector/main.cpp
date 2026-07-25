#include <android/sensor.h>
#include <sensor/SensorEventQueue.h>
#include <sensor/SensorManager.h>
#include <utils/String16.h>
#include <utils/String8.h>
#include <utils/SystemClock.h>

#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <string_view>
#include <thread>

namespace {

constexpr int kHalBypassReplayDataInjection = 4;
constexpr std::string_view kPackageName = "dev.hd.sensor_injector";

struct SensorTarget {
    int32_t handle;
    int32_t type;
    size_t value_count;
};

bool parse_i64(const char* text, int64_t* value) {
    char* end = nullptr;
    errno = 0;
    const long long parsed = std::strtoll(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') {
        return false;
    }
    *value = parsed;
    return true;
}

bool target_for(std::string_view name, SensorTarget* target) {
    if (name == "accelerometer") {
        *target = {1, ASENSOR_TYPE_ACCELEROMETER, 3};
    } else if (name == "gyroscope") {
        *target = {2, ASENSOR_TYPE_GYROSCOPE, 3};
    } else if (name == "magnetometer") {
        *target = {5, ASENSOR_TYPE_MAGNETIC_FIELD, 3};
    } else if (name == "light") {
        *target = {6, ASENSOR_TYPE_LIGHT, 1};
    } else if (name == "proximity") {
        *target = {7, ASENSOR_TYPE_PROXIMITY, 1};
    } else {
        return false;
    }
    return true;
}

}  // namespace

int main(int argc, char** argv) {
    if (argc < 4) {
        std::cerr << "usage: hd-sensor-injector SENSOR DURATION_MS VALUE_MICROUNITS...\n";
        return 2;
    }
    SensorTarget target{};
    if (!target_for(argv[1], &target) || argc != static_cast<int>(3 + target.value_count)) {
        std::cerr << "invalid sensor or value count\n";
        return 2;
    }
    int64_t duration_ms = 0;
    if (!parse_i64(argv[2], &duration_ms) || duration_ms < 0 || duration_ms > 3'600'000) {
        std::cerr << "invalid duration\n";
        return 2;
    }

    ASensorEvent event{};
    event.version = sizeof(event);
    event.sensor = target.handle;
    event.type = target.type;
    event.timestamp = android::elapsedRealtimeNano();
    for (size_t index = 0; index < target.value_count; ++index) {
        int64_t value = 0;
        if (!parse_i64(argv[3 + index], &value)) {
            std::cerr << "invalid sensor value\n";
            return 2;
        }
        event.data[index] = static_cast<float>(value) / 1'000'000.0F;
    }

    const android::String16 package_name(kPackageName.data());
    android::SensorManager& manager = android::SensorManager::getInstanceForPackage(package_name);
    android::sp<android::SensorEventQueue> queue = manager.createEventQueue(
            android::String8(kPackageName.data()), kHalBypassReplayDataInjection);
    if (queue == nullptr) {
        std::cerr << "failed to create HAL-bypass injection queue\n";
        return 3;
    }

    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(duration_ms);
    do {
        event.timestamp = android::elapsedRealtimeNano();
        const android::status_t status = queue->injectSensorEvent(event);
        if (status != android::OK) {
            std::cerr << "sensor injection failed: " << status << '\n';
            return 4;
        }
        if (duration_ms == 0) {
            // Keep the connection alive long enough for SensorService's looper to drain the tube.
            std::this_thread::sleep_for(std::chrono::milliseconds(100));
            break;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
    } while (std::chrono::steady_clock::now() < deadline);
    return 0;
}
