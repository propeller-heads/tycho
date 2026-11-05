// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/**
 * @title Propellerheads PrefixLengthEncoded Byte Array Library
 * @author PropellerHeads Developers
 * @dev Provide a gas efficient encoding for bytes array.
 *
 * Array of bytes are encoded as a single bytes like this :
 *       16 bits      ??bits       16bits        ??bits ...
 *   [length(elem1)]  [elem1]  [length(elem2)]  [elem2] ...
 *
 * This is much more efficient and compact than default solidity encoding.
 */
library LibPrefixLengthEncodedByteArray {
    /**
     * @dev Pop the first element of an array and returns it with the remaining data.
     */
    function next(
        bytes calldata encoded
    ) internal pure returns (bytes calldata elem, bytes calldata res) {
        assembly {
            switch iszero(encoded.length)
            case 1 {
                elem.offset := 0
                elem.length := 0
                res.offset := 0
                res.length := 0
            }
            default {
                let l := shr(240, calldataload(encoded.offset))
                elem.offset := add(encoded.offset, 2)
                elem.length := l
                res.offset := add(elem.offset, l)
                res.length := sub(sub(encoded.length, l), 2)
            }
        }
    }

    /**
     * @dev Gets the size of the encoded array.
     */
    function size(bytes calldata encoded) internal pure returns (uint256 s) {
        assembly {
            let offset := encoded.offset
            let end := add(encoded.offset, encoded.length)
            for {

            } lt(offset, end) {

            } {
                offset := add(offset, add(shr(240, calldataload(offset)), 2))
                s := add(s, 1)
            }
        }
    }

    /**
     * @dev Cast an encoded array into a Solidity array.
     */
    function toArray(
        bytes calldata encoded
    ) internal pure returns (bytes[] memory arr) {
        bytes calldata elem;
        uint256 idx = 0;
        arr = new bytes[](LibPrefixLengthEncodedByteArray.size(encoded));
        while (encoded.length > 0) {
            (elem, encoded) = LibPrefixLengthEncodedByteArray.next(encoded);
            arr[idx] = elem;
            idx++;
        }
    }
}
