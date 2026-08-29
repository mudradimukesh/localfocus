import 'package:flutter_test/flutter_test.dart';
import 'package:local_focus_mobile/companion_app.dart';

void main() {
  group('decodeConnectCode', () {
    test('decodes known codes back to their host URLs', () {
      // These pairings must match the dashboard's encodeConnectCode (JS).
      expect(decodeConnectCode('R2M0-2AMK'), 'http://192.168.1.42:4799');
      expect(decodeConnectCode('1800-008B'), 'http://10.0.0.1:4799');
      expect(decodeConnectCode('FW00-00C0'), 'http://127.0.0.1:4799');
      expect(decodeConnectCode('R2M0-85M2'), 'http://192.168.4.22:4799');
    });

    test('is tolerant of lowercase, spaces, and missing dash', () {
      expect(decodeConnectCode('r2m0-2amk'), 'http://192.168.1.42:4799');
      expect(decodeConnectCode('r2m0 2amk'), 'http://192.168.1.42:4799');
      expect(decodeConnectCode('R2M02AMK'), 'http://192.168.1.42:4799');
    });

    test('maps Crockford ambiguous characters (O->0, I/L->1)', () {
      // FW00-00C0 with the zeros typed as the letter O should still decode.
      expect(decodeConnectCode('FWOO-OOCO'), 'http://127.0.0.1:4799');
    });

    test('rejects codes that fail the checksum', () {
      // Last character altered so the checksum no longer matches.
      expect(decodeConnectCode('R2M0-2AMA'), isNull);
    });

    test('rejects codes of the wrong length or with invalid symbols', () {
      expect(decodeConnectCode('R2M0-2AM'), isNull); // too short
      expect(decodeConnectCode('R2M0-2AMKK'), isNull); // too long
      expect(decodeConnectCode(''), isNull);
    });
  });
}
