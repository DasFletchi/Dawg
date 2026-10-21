import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.file.Files;
import java.nio.file.Path;

/** Offline process fixture, not a Minecraft implementation. */
public final class DawgTestServer {
    public static void main(String[] args) throws Exception {
        Files.createDirectories(Path.of("world"));
        Files.writeString(Path.of("world", "level.dat"), "Dawg integration fixture\n");
        if (Files.exists(Path.of("startup-delay-ms"))) {
            System.out.println("Fixture startup paused");
            Thread.sleep(Long.parseLong(Files.readString(Path.of("startup-delay-ms")).trim()));
        }
        System.out.println("Done (0.001s)! For help, type \"help\"");
        System.err.println("Fixture stderr is connected");
        boolean slowStop = false;
        try (var input = new BufferedReader(new InputStreamReader(System.in))) {
            for (String command; (command = input.readLine()) != null;) {
                switch (command) {
                    case "stop" -> {
                        if (slowStop) {
                            Thread.sleep(250);
                        }
                        System.out.println("Stopping server");
                        Files.writeString(Path.of("world", "clean-shutdown.txt"), "Stopped through console\n");
                        return;
                    }
                    case "crash" -> {
                        System.err.println("Fixture intentional crash");
                        System.exit(42);
                    }
                    case "status" -> System.out.println("Fixture status: running");
                    case "slow-stop" -> {
                        slowStop = true;
                        System.out.println("Fixture slow stop enabled");
                    }
                    default -> System.out.println("Fixture received: " + command);
                }
            }
        }
    }
}
