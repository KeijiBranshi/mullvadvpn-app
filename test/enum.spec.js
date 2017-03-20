import { expect } from 'chai';
import Enum from '../app/lib/enum';

describe('enum', () => {
  it('should be able to compare values', () => {
    const e = new Enum('NORTH', 'SOUTH', 'WEST', 'EAST');
    expect(e.NORTH).to.be.equal('NORTH');
  });

  it('should not be able to modify enum', () => {
    const e = new Enum('NORTH', 'SOUTH', 'WEST', 'EAST');
    expect(() => e.ANYWHERE = 'ANYWHERE').to.throw();
  });

  it('should be able to validate enum keys', () => {
    const e = new Enum('NORTH', 'SOUTH', 'WEST', 'EAST');
    expect(e.isValid('SOUTH')).to.be.true;
    expect(e.isValid('ANYWHERE')).to.be.false;
    expect(e.isValid()).to.be.false;
    expect(e.isValid(null)).to.be.false;
  });

  it('should receive correct keys from Object.keys', () => {
    const keys = ['NORTH', 'SOUTH', 'WEST', 'EAST'];
    const e = new Enum(...keys);
    expect(Object.keys(e)).to.be.deep.equal(keys);
  });

  it('should do reverse lookup', () => {
    const keys = { NORTH: 0, SOUTH: 1, WEST: 2, EAST: 3 };
    const e = new Enum(keys);
    expect(e.reverse(2)).to.be.equal('WEST');
  });

  it('should return undefined on reverse lookup failure', () => {
    const keys = { NORTH: 0, SOUTH: 1, WEST: 2, EAST: 3 };
    const e = new Enum(keys);
    expect(e.reverse(1337)).to.be.undefined;
  });
});
