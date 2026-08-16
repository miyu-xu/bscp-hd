package com.hd.locationprobe;

import android.app.Activity;
import android.location.Location;
import android.location.LocationListener;
import android.location.LocationManager;
import android.os.Bundle;
import android.os.SystemClock;
import android.util.Log;
import android.view.WindowManager;
import android.widget.TextView;

import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Locale;

/**
 * Test-only framework subscriber used by the HD real-Guest release gates.
 *
 * <p>The probe never creates a mock provider. It subscribes to the real GPS provider and writes
 * the latest LocationManager callback to an owner-only app file that the gate reads with run-as.
 */
public final class LocationProbeActivity extends Activity implements LocationListener {
    private static final String TAG = "HDLocationProbe";
    private static final String OUTPUT_FILE = "location.txt";

    private LocationManager locationManager;
    private double expectedLatitude;
    private double expectedLongitude;
    private double expectedAltitude;
    private float expectedAccuracy;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        setShowWhenLocked(true);
        setTurnScreenOn(true);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        TextView status = new TextView(this);
        status.setText("Waiting for the real GPS provider…");
        setContentView(status);

        try {
            expectedLatitude = requiredDoubleExtra("expected_latitude");
            expectedLongitude = requiredDoubleExtra("expected_longitude");
            expectedAltitude = requiredDoubleExtra("expected_altitude");
            expectedAccuracy = (float) requiredDoubleExtra("expected_accuracy");
            locationManager = getSystemService(LocationManager.class);
            if (locationManager == null) {
                throw new IllegalStateException("LocationManager unavailable");
            }
            locationManager.requestLocationUpdates(
                    LocationManager.GPS_PROVIDER, 1000L, 0.0f, this);
            Log.i(TAG, "subscribed provider=gps interval_ms=1000");
        } catch (RuntimeException error) {
            Log.e(TAG, "subscription failed", error);
            writeResult("status=error error=" + sanitize(error.toString()) + "\n");
            finish();
        }
    }

    @Override
    protected void onDestroy() {
        Log.i(TAG, "onDestroy");
        if (locationManager != null) {
            locationManager.removeUpdates(this);
        }
        super.onDestroy();
    }

    @Override
    public void onLocationChanged(Location location) {
        boolean matched = close(location.getLatitude(), expectedLatitude, 0.0000001)
                && close(location.getLongitude(), expectedLongitude, 0.0000001)
                && location.hasAltitude()
                && close(location.getAltitude(), expectedAltitude, 0.001)
                && location.hasAccuracy()
                && close(location.getAccuracy(), expectedAccuracy, 0.001);
        String result = String.format(
                Locale.ROOT,
                "status=%s provider=%s latitude=%.7f longitude=%.7f altitude=%.3f "
                        + "accuracy=%.3f has_altitude=%s has_accuracy=%s "
                        + "elapsed_realtime_nanos=%d observed_at_millis=%d\n",
                matched ? "match" : "observed",
                location.getProvider(),
                location.getLatitude(),
                location.getLongitude(),
                location.getAltitude(),
                location.getAccuracy(),
                location.hasAltitude(),
                location.hasAccuracy(),
                location.getElapsedRealtimeNanos(),
                SystemClock.elapsedRealtime());
        writeResult(result);
        Log.i(TAG, result.trim());
        if (matched) {
            finish();
        }
    }

    @Override
    protected void onPause() {
        Log.i(TAG, "onPause");
        super.onPause();
    }

    @Override
    protected void onStop() {
        Log.i(TAG, "onStop");
        super.onStop();
    }

    private double requiredDoubleExtra(String name) {
        String value = getIntent().getStringExtra(name);
        if (value == null) {
            throw new IllegalArgumentException("missing " + name);
        }
        return Double.parseDouble(value);
    }

    private void writeResult(String value) {
        try (FileOutputStream output = openFileOutput(OUTPUT_FILE, MODE_PRIVATE)) {
            output.write(value.getBytes(StandardCharsets.UTF_8));
        } catch (IOException error) {
            throw new IllegalStateException("unable to write probe result", error);
        }
    }

    private static boolean close(double actual, double expected, double tolerance) {
        return Math.abs(actual - expected) <= tolerance;
    }

    private static String sanitize(String value) {
        return value.replace('\n', ' ').replace('\r', ' ');
    }
}
