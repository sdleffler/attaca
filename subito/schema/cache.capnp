@0x95a7e74e9af84091;

struct Entry {
    maybeHash :union {
        some @0 :Data;
        none @1 :Void;
    }

    inode :group {
        timestampNs @2 :Int64;

        generation @3 :UInt32;
        number @4 :UInt64;

        union {
            version @5 :UInt64;
            times :group {
                ctimeNs @6 :Int64;
                mtimeNs @7 :Int64;
            }
        }
    }
}
